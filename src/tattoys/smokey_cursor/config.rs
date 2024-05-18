//! All the variables that can be configured for the simulation

use glam::Vec2;

use super::particle::PARTICLE_SIZE;

/// All the config for the simulation
#[non_exhaustive]
pub struct Config {
    /// The gravitational exceleration of the system in metres per second
    pub gravity: Vec2,
    /// The velocity of a particle when it is first added
    pub initial_velocity: Vec2,
    /// How much bigger a partical is compared to a rendered pixel
    pub scale: f32,
}

impl Default for Config {
    #[allow(clippy::float_arithmetic)]
    fn default() -> Self {
        Self {
            gravity: Vec2::new(0.0, -9.81),
            initial_velocity: Vec2::ZERO,
            scale: PARTICLE_SIZE * 0.75,
        }
    }
}
