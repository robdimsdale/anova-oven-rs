use defmt::{info, warn};
use embassy_net::dns::DnsSocket;
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use reqwless::client::HttpClient;
use reqwless::headers::ContentType;
use reqwless::request::{Method, RequestBuilder};

use crate::display::celcius_to_fahrenheit;
use crate::SERVER_URL;

fn normalize_server_url(url: &str) -> alloc::string::String {
    let trimmed = url.trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.into()
    } else {
        alloc::format!("http://{trimmed}")
    }
}

pub async fn fetch_and_log_status(
    stack: embassy_net::Stack<'static>,
    rx_buf: &mut [u8],
) -> Option<anova_oven_api::OvenStatus> {
    let client_state = TcpClientState::<1, 1024, 1024>::new();
    let tcp = TcpClient::new(stack, &client_state);
    let dns = DnsSocket::new(stack);
    let mut client = HttpClient::new(&tcp, &dns);

    let server = normalize_server_url(SERVER_URL);
    let url = alloc::format!("{server}/status");
    let mut request = match client.request(Method::GET, &url).await {
        Ok(r) => r,
        Err(_) => {
            warn!("GET /status: connection failed");
            return None;
        }
    };

    let response = match request.send(rx_buf).await {
        Ok(r) => r,
        Err(_) => {
            warn!("GET /status: send failed");
            return None;
        }
    };

    if response.status.0 != 200 {
        warn!("GET /status: HTTP {}", response.status.0);
        return None;
    }

    let body = match response.body().read_to_end().await {
        Ok(b) => b,
        Err(_) => {
            warn!("GET /status: failed to read body");
            return None;
        }
    };

    match serde_json::from_slice::<anova_oven_api::OvenStatus>(body) {
        Ok(status) => {
            info!(
                "Status: mode={} temp={}F target={}F steam={}% door={} water={}",
                status.mode.as_str(),
                celcius_to_fahrenheit(status.current_temperature_c()),
                celcius_to_fahrenheit(status.target_temperature_c.unwrap_or(0.0)),
                status.steam_pct,
                status.door_open,
                status.water_tank_empty,
            );
            Some(status)
        }
        Err(_) => {
            warn!("GET /status: failed to parse JSON");
            None
        }
    }
}

pub async fn fetch_current_cook(
    stack: embassy_net::Stack<'static>,
    rx_buf: &mut [u8],
) -> Option<anova_oven_api::CurrentCook> {
    let client_state = TcpClientState::<1, 1024, 1024>::new();
    let tcp = TcpClient::new(stack, &client_state);
    let dns = DnsSocket::new(stack);
    let mut client = HttpClient::new(&tcp, &dns);

    let server = normalize_server_url(SERVER_URL);
    let url = alloc::format!("{server}/current-cook");
    let mut request = match client.request(Method::GET, &url).await {
        Ok(r) => r,
        Err(_) => {
            warn!("GET /current-cook: connection failed");
            return None;
        }
    };

    let response = match request.send(rx_buf).await {
        Ok(r) => r,
        Err(_) => {
            warn!("GET /current-cook: send failed");
            return None;
        }
    };

    if response.status.0 == 204 {
        return None;
    }
    if response.status.0 != 200 {
        warn!("GET /current-cook: HTTP {}", response.status.0);
        return None;
    }

    let body = match response.body().read_to_end().await {
        Ok(b) => b,
        Err(_) => {
            warn!("GET /current-cook: failed to read body");
            return None;
        }
    };

    match serde_json::from_slice::<anova_oven_api::CurrentCook>(body) {
        Ok(cook) => {
            info!(
                "Current cook: {} ({} stages)",
                cook.recipe_title.as_str(),
                cook.total_stage_count,
            );
            Some(cook)
        }
        Err(_) => {
            warn!("GET /current-cook: failed to parse JSON");
            None
        }
    }
}

pub async fn send_stop(stack: embassy_net::Stack<'static>, rx_buf: &mut [u8]) {
    let client_state = TcpClientState::<1, 1024, 1024>::new();
    let tcp = TcpClient::new(stack, &client_state);
    let dns = DnsSocket::new(stack);
    let mut client = HttpClient::new(&tcp, &dns);

    let server = normalize_server_url(SERVER_URL);
    let url = alloc::format!("{server}/stop");
    let mut request = match client.request(Method::POST, &url).await {
        Ok(r) => r,
        Err(_) => {
            warn!("POST /stop: connection failed");
            return;
        }
    };

    let response = match request.send(rx_buf).await {
        Ok(r) => r,
        Err(_) => {
            warn!("POST /stop: send failed");
            return;
        }
    };

    if response.status.0 >= 200 && response.status.0 < 300 {
        info!("POST /stop: success (HTTP {})", response.status.0);
    } else {
        warn!("POST /stop: HTTP {}", response.status.0);
    }
}

pub async fn send_start(stack: embassy_net::Stack<'static>, rx_buf: &mut [u8], recipe_id: &str) {
    let client_state = TcpClientState::<1, 1024, 1024>::new();
    let tcp = TcpClient::new(stack, &client_state);
    let dns = DnsSocket::new(stack);
    let mut client = HttpClient::new(&tcp, &dns);

    let server = normalize_server_url(SERVER_URL);
    let url = alloc::format!("{server}/start");
    let request = match client.request(Method::POST, &url).await {
        Ok(r) => r,
        Err(_) => {
            warn!("POST /start: connection failed");
            return;
        }
    };

    // Build JSON body: {"recipe_id": "..."}
    let body = alloc::format!(r#"{{"recipe_id":"{}"}}"#, recipe_id);
    let mut request = request
        .body(body.as_bytes())
        .content_type(ContentType::ApplicationJson);

    let response = match request.send(rx_buf).await {
        Ok(r) => r,
        Err(_) => {
            warn!("POST /start: send failed");
            return;
        }
    };

    if response.status.0 >= 200 && response.status.0 < 300 {
        info!("POST /start: success (HTTP {})", response.status.0);
    } else {
        warn!("POST /start: HTTP {}", response.status.0);
    }
}

pub async fn fetch_and_log_recipes(
    stack: embassy_net::Stack<'static>,
    rx_buf: &mut [u8],
) -> alloc::vec::Vec<anova_oven_api::Recipe> {
    let client_state = TcpClientState::<1, 4096, 4096>::new();
    let tcp = TcpClient::new(stack, &client_state);
    let dns = DnsSocket::new(stack);
    let mut client = HttpClient::new(&tcp, &dns);

    let server = normalize_server_url(SERVER_URL);
    let url = alloc::format!("{server}/recipes");
    let mut request = match client.request(Method::GET, &url).await {
        Ok(r) => r,
        Err(_) => {
            warn!("GET /recipes: connection failed");
            return alloc::vec::Vec::new();
        }
    };

    let response = match request.send(rx_buf).await {
        Ok(r) => r,
        Err(_) => {
            warn!("GET /recipes: send failed");
            return alloc::vec::Vec::new();
        }
    };

    if response.status.0 != 200 {
        warn!("GET /recipes: HTTP {}", response.status.0);
        return alloc::vec::Vec::new();
    }

    let body = match response.body().read_to_end().await {
        Ok(b) => b,
        Err(_) => {
            warn!("GET /recipes: failed to read body");
            return alloc::vec::Vec::new();
        }
    };

    match serde_json::from_slice::<alloc::vec::Vec<anova_oven_api::Recipe>>(body) {
        Ok(mut recipes) => {
            // Normalize all recipes for Anova compatibility
            for recipe in &mut recipes {
                recipe.normalize();
            }
            info!("Recipes: {} found", recipes.len());
            for recipe in &recipes {
                info!(
                    "  - {} ({} stages)",
                    recipe.title.as_str(),
                    recipe.stage_count
                );
            }
            recipes
        }
        Err(_) => {
            warn!("GET /recipes: failed to parse JSON");
            alloc::vec::Vec::new()
        }
    }
}
