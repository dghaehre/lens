use crate::error::LumenError;
use dirs::home_dir;
use serde::Deserialize;
use serde_json::from_reader;
use std::fs::File;
use std::io::BufReader;

use crate::Cli;

#[derive(Debug, Deserialize, Default)]
pub struct LumenConfig {
    #[serde(default)]
    pub theme: Option<String>,
}

fn default_config_path() -> Option<String> {
    home_dir().and_then(|mut path| {
        path.push(".config/lumen/lumen.config.json");
        path.exists()
            .then_some(path)
            .and_then(|p| p.to_str().map(|s| s.to_string()))
    })
}

impl LumenConfig {
    pub fn build(cli: &Cli) -> Result<Self, LumenError> {
        let config = if let Some(config_path) = &cli.config {
            LumenConfig::from_file(config_path)?
        } else {
            match default_config_path() {
                Some(path) => LumenConfig::from_file(&path)?,
                None => LumenConfig::default(),
            }
        };

        Ok(LumenConfig {
            theme: config.theme,
        })
    }

    pub fn from_file(file_path: &str) -> Result<Self, LumenError> {
        let file = File::open(file_path)?;
        let reader = BufReader::new(file);

        // Deserialize JSON data into the LumenConfig struct
        let config: LumenConfig = match from_reader(reader) {
            Ok(config) => config,
            Err(e) => return Err(LumenError::InvalidConfiguration(e.to_string())),
        };

        Ok(config)
    }
}
