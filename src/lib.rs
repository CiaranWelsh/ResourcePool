use std::env;

mod resource_pool;
mod init_logger;
mod yaml_serializable;
mod observable;

pub fn get_observability_server_address() -> String {
    // Get the environment variable, defaulting to "127.0.0.1:5000" if not found
    let default_address = "http://127.0.0.1:5000".to_string();
    let env_var_key = "OBSERVABILITY_IP_ADDRESS";
    let server_address: String = env::var(env_var_key).unwrap_or(default_address);
    server_address
}

