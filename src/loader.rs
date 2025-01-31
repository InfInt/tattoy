//! The manager of all the fancy Tattoy eye-candy code

use std::sync::Arc;

use color_eyre::eyre::Result;

use tokio::sync::mpsc;

use crate::run::{FrameUpdate, Protocol};
use crate::shared_state::SharedState;
use crate::tattoys::index::{create_instance, Tattoyer};

/// The number of microseonds in a second
const ONE_MICROSECOND: u64 = 1_000_000;

/// Rename to "Compositor" or "Tattoys"?
pub(crate) struct Loader {
    /// All the enabled tattoys that will be run
    tattoys: Vec<Box<dyn Tattoyer + Send>>,
}

impl Loader {
    /// Create a Compositor/Tattoy
    pub fn new(state: &Arc<SharedState>, requested_tattoys: Vec<String>) -> Result<Self> {
        let mut tattoys: Vec<Box<dyn Tattoyer + Send>> = vec![];
        for tattoy in requested_tattoys {
            let n = create_instance(&tattoy, state)?;
            tattoys.push(n);
        }
        if tattoys.is_empty() {
            return Err(color_eyre::eyre::eyre!("No tattoys to run"));
        }
        Ok(Self { tattoys })
    }

    /// Run the tattoy(s)
    pub fn run(
        &mut self,
        tattoy_output: &mpsc::Sender<FrameUpdate>,
        mut protocol: tokio::sync::broadcast::Receiver<Protocol>,
    ) -> Result<()> {
        let target_frame_rate = 30;

        let target = ONE_MICROSECOND.wrapping_div(target_frame_rate);
        let target_frame_rate_micro = std::time::Duration::from_micros(target);

        loop {
            let frame_time = std::time::Instant::now();

            // TODO: should this be oneshot?
            if let Ok(message) = protocol.try_recv() {
                match message {
                    Protocol::End => {
                        break;
                    }
                    Protocol::Resize { width, height } => {
                        for tattoy in &mut self.tattoys {
                            tattoy.set_tty_size(width, height);
                        }
                    }
                };
            }

            for tattoy in &mut self.tattoys {
                let surface = tattoy.tick()?;
                tattoy_output.try_send(FrameUpdate::TattoySurface(surface))?;
            }

            if let Some(i) = target_frame_rate_micro.checked_sub(frame_time.elapsed()) {
                std::thread::sleep(i);
            }
        }

        tracing::debug!("Tattoy loop finished");
        Ok(())
    }
}
