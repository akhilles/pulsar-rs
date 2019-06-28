use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use futures::{Future, future::{self, Either}};
use rand;
use serde::Serialize;
use serde_json;
use tokio::prelude::*;
use tokio::runtime::TaskExecutor;

use crate::client::SerializeMessage;
use crate::connection::{Authentication, Connection, SerialId};
use crate::error::{ConnectionError, ProducerError};
use crate::message::proto::{self, EncryptionKeys};
use crate::{Pulsar, Error};
use futures::sync::oneshot;
use futures::sync::mpsc::{unbounded, UnboundedSender, UnboundedReceiver};

type ProducerId = u64;
type ProducerName = String;

#[derive(Debug, Clone, Default)]
pub struct Message {
    pub payload: Vec<u8>,
    pub properties: HashMap<String, String>,
    ///key to decide partition for the msg
    pub partition_key: ::std::option::Option<String>,
    /// Override namespace's replication
    pub replicate_to: ::std::vec::Vec<String>,
    pub compression: ::std::option::Option<i32>,
    pub uncompressed_size: ::std::option::Option<u32>,
    /// Removed below checksum field from Metadata as
    /// it should be part of send-command which keeps checksum of header + payload
    ///optional sfixed64 checksum = 10;
    /// differentiate single and batch message metadata
    pub num_messages_in_batch: ::std::option::Option<i32>,
    /// the timestamp that this event occurs. it is typically set by applications.
    /// if this field is omitted, `publish_time` can be used for the purpose of `event_time`.
    pub event_time: ::std::option::Option<u64>,
    /// Contains encryption key name, encrypted key and metadata to describe the key
    pub encryption_keys: ::std::vec::Vec<EncryptionKeys>,
    /// Algorithm used to encrypt data key
    pub encryption_algo: ::std::option::Option<String>,
    /// Additional parameters required by encryption
    pub encryption_param: ::std::option::Option<Vec<u8>>,
    pub schema_version: ::std::option::Option<Vec<u8>>,
}

#[derive(Clone)]
pub struct MultiTopicProducer {
    message_sender: UnboundedSender<ProducerMessage>,
}

impl MultiTopicProducer {
    pub fn new(pulsar: Pulsar) -> MultiTopicProducer {
        let (tx, rx) = unbounded();
        let executor = pulsar.executor().clone();
        executor.spawn(ProducerEngine {
            pulsar,
            inbound: rx,
            producers: BTreeMap::new(),
            new_producers: BTreeMap::new(),
        });
        MultiTopicProducer {
            message_sender: tx,
        }
    }

    pub fn send<T: SerializeMessage, S: Into<String>>(&self, topic: S, message: &T) -> impl Future<Item=proto::CommandSendReceipt, Error=ProducerError> {
        match T::serialize_message(message) {
            Ok(message) => {
                let (resolver, future) = oneshot::channel();
                match self.message_sender.unbounded_send(ProducerMessage {
                    topic: topic.into(),
                    message,
                    resolver
                }) {
                    Ok(_) => Either::A(future.then(|r| match r {
                        Ok(Ok(data)) => Ok(data),
                        Ok(Err(e)) => Err(e),
                        Err(oneshot::Canceled) => Err(ProducerError::Custom("Unexpected error: pulsar producer engine unexpectedly dropped".to_owned()))
                    })),
                    Err(_) => Either::B(future::failed(ProducerError::Custom("Unexpected error: pulsar producer engine unexpectedly dropped".to_owned())))
                }
            },
            Err(e) => Either::B(future::failed(e))
        }
    }
}

struct ProducerEngine {
    pulsar: Pulsar,
    inbound: UnboundedReceiver<ProducerMessage>,
    producers: BTreeMap<String, Arc<Producer>>,
    new_producers: BTreeMap<String, oneshot::Receiver<Result<Arc<Producer>, Error>>>,
}

impl Future for ProducerEngine {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Result<Async<Self::Item>, Self::Error> {
        if !self.new_producers.is_empty() {
            let mut resolved_topics = Vec::new();
            for (topic, producer) in self.new_producers.iter_mut() {
                match producer.poll() {
                    Ok(Async::Ready(Ok(producer))) => {
                        self.producers.insert(producer.topic().to_owned(), producer);
                        resolved_topics.push(topic.clone());
                    }
                    Ok(Async::Ready(Err(_))) | Err(_) => resolved_topics.push(topic.clone()),
                    Ok(Async::NotReady) => {},
                }
            }
            for topic in resolved_topics {
                self.new_producers.remove(&topic);
            }
        }

        loop {
            match try_ready!(self.inbound.poll()) {
                Some(ProducerMessage { topic, message, resolver }) => {
                    match self.producers.get(&topic) {
                        Some(producer) => {
                            tokio::spawn(producer.send_message(message, None)
                                 .then(|r| resolver.send(r).map_err(drop)));
                        }
                        None => {
                            let pending = self.new_producers.remove(&topic)
                                .unwrap_or_else(|| {
                                    let (tx, rx) = oneshot::channel();
                                    tokio::spawn({
                                        self.pulsar.create_producer(topic.clone(), None)
                                            .then(|r| tx.send(r.map(|producer| Arc::new(producer))).map_err(drop))
                                    });
                                    rx
                                });
                            let (tx, rx) = oneshot::channel();
                            tokio::spawn(pending.map_err(drop).and_then(move |r| match r {
                                Ok(producer) => {
                                    let _ = tx.send(Ok(producer.clone()));
                                    Either::A(producer.send_message(message, None)
                                        .then(|r| resolver.send(r))
                                        .map_err(drop)
                                    )
                                }
                                Err(e) => {
                                    // TODO find better error propogation here
                                    let _ = resolver.send(Err(ProducerError::Custom(e.to_string())));
                                    let _ = tx.send(Err(e));
                                    Either::B(future::failed(()))
                                }
                            }));
                            self.new_producers.insert(topic, rx);
                        }
                    }
                }
                None => return Ok(Async::Ready(()))
            }
        }
    }
}

struct ProducerMessage {
    topic: String,
    message: Message,
    resolver: oneshot::Sender<Result<proto::CommandSendReceipt, ProducerError>>,
}

pub struct Producer {
    connection: Arc<Connection>,
    id: ProducerId,
    name: ProducerName,
    topic: String,
    message_id: SerialId,
}

impl Producer {
    pub fn new<S1, S2>(
        addr: S1,
        topic: S2,
        name: Option<String>,
        auth: Option<Authentication>,
        proxy_to_broker_url: Option<String>,
        executor: TaskExecutor,
    ) -> impl Future<Item=Producer, Error=ConnectionError>
        where S1: Into<String>,
              S2: Into<String>,
    {
        Connection::new(addr.into(), auth, proxy_to_broker_url, executor)
            .and_then(move |conn| Producer::from_connection(Arc::new(conn), topic.into(), name))
    }

    pub fn from_connection<S: Into<String>>(connection: Arc<Connection>, topic: S, name: Option<String>) -> impl Future<Item=Producer, Error=ConnectionError> {
        let topic = topic.into();
        let producer_id = rand::random();
        let sequence_ids = SerialId::new();

        let sender = connection.sender().clone();
        connection.sender().lookup_topic(topic.clone(), false)
            .and_then({
                let topic = topic.clone();
                move |_| sender.create_producer(topic.clone(), producer_id, name)
            })
            .map(move |success| {
                Producer {
                    connection,
                    id: producer_id,
                    name: success.producer_name,
                    topic,
                    message_id: sequence_ids,
                }
            })
    }

    pub fn is_valid(&self) -> bool {
        self.connection.is_valid()
    }

    pub fn topic(&self) -> &str {
        &self.topic
    }

    pub fn check_connection(&self) -> impl Future<Item=(), Error=ConnectionError> {
        self.connection.sender().lookup_topic("test", false)
            .map(|_| ())
    }

    pub fn send_raw(&self, data: Vec<u8>, properties: Option<HashMap<String, String>>) -> impl Future<Item=proto::CommandSendReceipt, Error=ConnectionError> {
        self.connection.sender().send(
            self.id,
            self.name.clone(),
            self.message_id.get(),
            None,
            Message { payload: data, properties: properties.unwrap_or_else(|| HashMap::new()), ..Default::default() },
        )
    }

    pub fn send<T: SerializeMessage>(&self, message: &T, num_messages: Option<i32>) -> impl Future<Item=proto::CommandSendReceipt, Error=ProducerError> {
        match T::serialize_message(message) {
            Ok(message) => Either::A(self.send_message(message, num_messages)),
            Err(e) => Either::B(future::failed(e))
        }
    }

    pub fn send_json<T: Serialize>(&mut self, msg: &T, properties: Option<HashMap<String, String>>) -> impl Future<Item=proto::CommandSendReceipt, Error=ProducerError> {
        let data = match serde_json::to_vec(msg) {
            Ok(data) => data,
            Err(e) => return Either::A(future::failed(e.into())),
        };
        Either::B(self.send_raw(data, properties).map_err(|e| e.into()))
    }

    pub fn error(&self) -> Option<ConnectionError> {
        self.connection.error()
    }

    fn send_message(&self, message: Message, num_messages: Option<i32>) -> impl Future<Item=proto::CommandSendReceipt, Error=ProducerError> {
        self.connection.sender().send(self.id, self.name.clone(), self.message_id.get(), num_messages, message)
            .map_err(|e| e.into())
    }
}

impl Drop for Producer {
    fn drop(&mut self) {
        let _ = self.connection.sender().close_producer(self.id);
    }
}
