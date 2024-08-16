use std::error::Error;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use log::trace;
use reqwest::blocking::Client;
use reqwest::Url;
use serde_json::json;
use crate::get_observability_server_address;
use crate::yaml_serializable::JsonSerializable;

pub trait Observable: JsonSerializable {
    fn send_to_server(&self) -> Result<(), Box<dyn Error>> {
        let client = Client::new();

        let mut serialized_data = serde_json::to_value(self)?;
        let current_time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        // Add the timestamp to the serialized data
        if let Some(serialized_data) = serialized_data.as_object_mut() {
            serialized_data.insert("timestamp".to_string(), json!(current_time));
        }

        let serialized_string = serde_json::to_string(&serialized_data)?;
        // warn!("serialized string: {:?}", serialized_string);

        let response = client.post(self.url())
            .header("Content-Type", "application/json")
            .body(serialized_string)
            .send()?;

        Ok(())
    }

    fn url(&self) -> Url;

    fn build_url_from_str(&self, endpoint: &str) -> Url {
        let url = get_observability_server_address() + endpoint;
        trace!("url is: {:?}", url);
        Url::from_str(url.as_str()).expect("Failed to parse")
    }
}
