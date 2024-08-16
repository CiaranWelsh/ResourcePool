use serde::{Serialize};
use serde::de::DeserializeOwned;
use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

pub trait YamlSerializable: Serialize + DeserializeOwned {
    fn to_yaml(&self) -> Result<String, Box<dyn Error>> {
        serde_yaml::to_string(self).map_err(Into::into)
    }

    fn from_yaml(yaml: &str) -> Result<Self, Box<dyn Error>> {
        serde_yaml::from_str(yaml).map_err(Into::into)
    }


    fn read_from_yaml_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn Error>> {
        let mut file = fs::File::open(path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        Self::from_yaml(&contents)
    }

    fn write_to_yaml_file<P: AsRef<Path>>(&self, path: P) -> Result<(), Box<dyn Error>> {
        let mut file = fs::File::create(path)?;
        let yaml = self.to_yaml()?;
        file.write_all(yaml.as_bytes())?;
        Ok(())
    }
}
pub trait JsonSerializable: Serialize + DeserializeOwned {
    fn to_json(&self) -> Result<String, Box<dyn Error>> {
        serde_json::to_string(self).map_err(Into::into)
    }

    fn from_json(json: &str) -> Result<Self, Box<dyn Error>> {
        serde_json::from_str(json).map_err(Into::into)
    }

    fn read_from_json_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn Error>> {
        let mut file = fs::File::open(path)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        Self::from_json(&contents)
    }

    fn write_to_json_file<P: AsRef<Path>>(&self, path: P) -> Result<(), Box<dyn Error>> {
        let mut file = fs::File::create(path)?;
        let json = self.to_json()?;
        file.write_all(json.as_bytes())?;
        Ok(())
    }
}