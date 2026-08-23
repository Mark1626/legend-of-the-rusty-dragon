use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use std::{println, time::Duration, todo};
use tokio::time::sleep;

pub type EventCallback = Box<dyn Fn(usize) + Send + 'static>;
pub type SharedNetwork = Arc<dyn Network + Send + Sync>;

#[async_trait]
pub trait Network {
    fn broadcast(&self, msg: &str);
    fn reply(&self, msg: &str);
    async fn add_event(&self, callback: EventCallback, delay: u64) -> Result<()>;
}

#[async_trait]
pub trait NetworkExt: Network {
    async fn on_event<F>(&self, cb: F, delay: u64) -> Result<()>
    where
        F: Fn(usize) + Send + 'static,
    {
        self.add_event(Box::new(cb), delay).await
    }
}

impl<T: Network + ?Sized> NetworkExt for T {}

pub struct DebugNetwork {}

impl DebugNetwork {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl Network for DebugNetwork {
    fn broadcast(&self, msg: &str) {
        println!("{}", msg.to_string());
    }

    fn reply(&self, msg: &str) {
        println!("[reply]: {}", msg.to_string());
    }

    async fn add_event(&self, callback: EventCallback, delay: u64) -> Result<()> {
        sleep(Duration::from_millis(delay)).await;
        tokio::task::spawn_blocking(move || {
            callback(10);
        })
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// `on_event` is only reachable here via the blanket impl on `dyn Network`,
    /// and the closure only reaches `DebugNetwork` via the vtable.
    #[tokio::test]
    async fn ext_trait_reaches_through_the_trait_object() {
        let network: SharedNetwork = Arc::new(DebugNetwork::new());

        let seen = Arc::new(AtomicUsize::new(0));
        let sink = Arc::clone(&seen);

        network
            .on_event(
                move |n| {
                    sink.store(n, Ordering::Relaxed);
                },
                1,
            )
            .await
            .unwrap();

        assert_eq!(seen.load(Ordering::Relaxed), 10);
    }
}

pub struct IrohGossipNetwork {}

#[async_trait]
impl Network for IrohGossipNetwork {
    fn broadcast(&self, msg: &str) {
        todo!();
    }

    fn reply(&self, msg: &str) {
        todo!()
    }

    async fn add_event(&self, callback: EventCallback, delay: u64) -> Result<()> {
        todo!()
    }
}
