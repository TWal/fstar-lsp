use tokio::sync::mpsc;
use tokio::sync::oneshot;
use anyhow::Result;

#[derive (Clone)]
pub struct Sender<T> {
    sender: mpsc::UnboundedSender<(T, oneshot::Sender<()>)>,
}

pub struct Receiver<T> {
    receiver: mpsc::UnboundedReceiver<(T, oneshot::Sender<()>)>,
}

pub struct Ack {
    sender: oneshot::Sender<()>,
}

impl<T> Sender<T> {
    pub async fn send(&self, x: T) -> Result<()> {
        let (send, recv) = oneshot::channel();
        //TODO: unwrap because type error otherwise
        self.sender.send((x, send)).unwrap();
        recv.await?;
        Ok(())
    }
}

impl<T> Receiver<T> {
    pub async fn recv(&mut self) -> Option<(T, Ack)> {
        self.receiver.recv().await.map(|(res, sender)| {
            (res, Ack{  sender })
        })
    }
}

impl Ack {
    pub fn ack(self) {
        let _ = self.sender.send(());
    }
}

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (Sender { sender }, Receiver { receiver })
}
