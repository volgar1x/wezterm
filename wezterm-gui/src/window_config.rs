use ::window::configuration::WindowConfiguration;
use config::configuration;

pub struct ConfigBridge;

impl WindowConfiguration for ConfigBridge {
    fn use_ime(&self) -> bool {
        configuration().use_ime
    }
}