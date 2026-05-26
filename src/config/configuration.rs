use crate::error::LensError;
use dirs::home_dir;
use serde::Deserialize;
use serde_json::from_reader;
use std::fs::File;
use std::io::BufReader;

use crate::Cli;

#[derive(Debug, Deserialize, Default)]
pub struct LensConfig {
    #[serde(default)]
    pub theme: Option<String>,
}

fn default_config_path() -> Option<String> {
    home_dir().and_then(|mut path| {
        path.push(".config/lens/lens.config.json");
        path.exists()
            .then_some(path)
            .and_then(|p| p.to_str().map(|s| s.to_string()))
    })
}

impl LensConfig {
    pub fn build(cli: &Cli) -> Result<Self, LensError> {
        let config = if let Some(config_path) = &cli.config {
            LensConfig::from_file(config_path)?
        } else {
            match default_config_path() {
                Some(path) => LensConfig::from_file(&path)?,
                None => LensConfig::default(),
            }
        };

        Ok(LensConfig {
            theme: config.theme,
        })
    }

    pub fn from_file(file_path: &str) -> Result<Self, LensError> {
        let file = File::open(file_path)?;
        let reader = BufReader::new(file);

        // Deserialize JSON data into the LensConfig struct
        let config: LensConfig = match from_reader(reader) {
            Ok(config) => config,
            Err(e) => return Err(LensError::InvalidConfiguration(e.to_string())),
        };

        Ok(config)
    }
}
