use super::*;
use super::bare_ide as bare;
use tokio::sync::mpsc;
use crate::sync_channel;
use std::ffi::OsStr;
use std::sync::{Arc, Mutex};
use anyhow::Result;

use tracing::{
    trace,
    debug,
    info,
    warn,
    error,
};



#[derive (Copy, Clone)]
enum LastMessageDecider {
    FullBuffer,
    Other,
}

impl LastMessageDecider {
    fn new(query: &Query) -> Self {
        match query {
            Query::FullBuffer(_) => LastMessageDecider::FullBuffer,
            _ => LastMessageDecider::Other,
        }
    }

    fn decide(&self, resp_or_msg: &ResponseOrMessage) -> bool {
        match self {
            LastMessageDecider::Other => {
                match resp_or_msg {
                    ResponseOrMessage::Response(_) => true,
                    _ => false,
                }
            }
            LastMessageDecider::FullBuffer => {
                match resp_or_msg {
                    ResponseOrMessage::Message(Message::Progress(ProgressMessageOrNull::Some(ProgressMessage::FullBufferFinished))) => true,
                    _ => false,
                }
            }
        }
    }
}

#[derive (Clone)]
enum FStarIDESender {
    NoSync(mpsc::Sender<ResponseOrMessage>),
    Sync(sync_channel::Sender<ResponseOrMessage>),
}

impl FStarIDESender {
    async fn send(&self, x: ResponseOrMessage) -> Result<()> {
        match self {
            FStarIDESender::NoSync(send) => {
                send.send(x).await?;
                Ok(())
            }
            FStarIDESender::Sync(send) => {
                send.send(x).await
            }
        }
    }
}

pub struct FStarIDE {
    send: bare::Sender,
    current_query_id: u64,
    channels: Arc<Mutex<std::collections::HashMap<String, (FStarIDESender, LastMessageDecider)>>>,
}

impl FStarIDE {
    pub fn new<S, I>(path_to_fstar_exe: S, args: I, cwd: &std::path::Path) -> Self
    where
        S: AsRef<OsStr>,
        I: IntoIterator<Item = S>,
    {
        let (send, recv) = bare::channel(path_to_fstar_exe, args, cwd);
        let channels = Arc::new(Mutex::new(std::collections::HashMap::new()));
        tokio::spawn(Self::receive_loop(recv, channels.clone()));
        FStarIDE {
            send,
            current_query_id: 0,
            channels,
        }
    }

    #[tracing::instrument(skip(self))]
    pub fn send_query_nosync(&mut self, query: Query) -> mpsc::Receiver<ResponseOrMessage> {
        let (send, recv) = mpsc::channel(100);
        self.send_query_internal(query, FStarIDESender::NoSync(send));
        recv
    }

    #[tracing::instrument(skip(self))]
    pub fn send_query_sync(&mut self, query: Query) -> sync_channel::Receiver<ResponseOrMessage> {
        let (send, recv) = sync_channel::channel();
        self.send_query_internal(query, FStarIDESender::Sync(send));
        recv
    }

    fn send_query_internal(&mut self, query: Query, send: FStarIDESender) {
        let query_id = self.current_query_id.to_string();
        self.current_query_id += 1;
        let last_message_decider = LastMessageDecider::new(&query);
        let full_query = bare::FullQuery {
            query_id: query_id.clone(),
            query,
        };
        self.send.send(full_query).unwrap();
        self.channels.lock().unwrap().insert(query_id, (send, last_message_decider));
    }

    fn normalize_query_id(query_id: &str) -> &str {
        match query_id.split_once('.') {
            None => query_id,
            Some((x, _)) => x,
        }
    }

    #[tracing::instrument(skip_all)]
    async fn receive_loop(mut recv: bare::Receiver, channels: Arc<Mutex<std::collections::HashMap<String, (FStarIDESender, LastMessageDecider)>>>,) -> Result<()> {
        while let Some(data) = recv.recv().await {
            let opt_send = channels.lock().unwrap().get(Self::normalize_query_id(&data.query_id)).cloned();
            if let Some((send, last_message_decider)) = opt_send {
                let is_last_data = last_message_decider.decide(&data.response_or_message);
                send.send(data.response_or_message).await?;
                if is_last_data  {
                    debug!("deleting channel for query {}", Self::normalize_query_id(&data.query_id));
                    channels.lock().unwrap().remove(&data.query_id);
                }
            } else {
                error!("no send in the channels map!")
            }
        }
        Ok(())
    }
}
