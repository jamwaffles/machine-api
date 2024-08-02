//! The Bambu MQTT client.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use dashmap::DashMap;
use tokio::sync::Mutex;

use crate::{
    command::Command,
    message::{Message, Print, PrintCommand},
    parser::parse_message,
    sequence_id::SequenceId,
};

/// The Bambu MQTT client.
#[derive(Clone)]
pub struct Client {
    /// The MQTT host.
    pub host: String,
    /// The access code.
    pub access_code: String,
    /// The serial number.
    pub serial: String,

    client: rumqttc::AsyncClient,
    event_loop: Arc<Mutex<rumqttc::EventLoop>>,

    responses: Arc<DashMap<SequenceId, Message>>,

    topic_device_request: String,
    topic_device_report: String,
}

const MAX_PACKET_SIZE: usize = 1024 * 1024;

impl Client {
    /// Creates a new Bambu printer MQTT client.
    pub fn new<S: Into<String> + Clone>(ip: S, access_code: S, serial: S) -> Result<Self> {
        let access_code = access_code.into();
        let serial = serial.into();
        let host = format!("mqtts://{}:8883", ip.clone().into());

        let client_id = format!("bambu-api-{}", nanoid::nanoid!(8));

        let ssl_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(crate::no_auth::NoAuth::new()))
            .with_no_client_auth();

        let mut opts = rumqttc::MqttOptions::new(client_id, ip, 8883);
        opts.set_max_packet_size(MAX_PACKET_SIZE, MAX_PACKET_SIZE);
        opts.set_keep_alive(Duration::from_secs(5));
        opts.set_credentials("bblp", &access_code);
        opts.set_transport(rumqttc::Transport::Tls(rumqttc::TlsConfiguration::Rustls(Arc::new(
            ssl_config,
        ))));

        let (client, event_loop) = rumqttc::AsyncClient::new(opts, 25);

        Ok(Self {
            host,
            access_code,
            topic_device_request: format!("device/{}/request", &serial),
            topic_device_report: format!("device/{}/report", &serial),
            serial,
            client,
            event_loop: Arc::new(Mutex::new(event_loop)),
            responses: Arc::new(DashMap::new()),
        })
    }

    /// Polls for a message from the MQTT event loop.
    /// You need to poll periodically to receive messages
    /// and to keep the connection alive.
    /// This function also handles reconnects.
    ///
    /// **NOTE** Don't block this while iterating
    ///
    /// # Errors
    ///
    /// Returns an error if there was a problem polling for a message or parsing the event.
    async fn poll(&mut self) -> Result<()> {
        let msg_opt = self.event_loop.lock().await.poll().await?;

        let message = parse_message(&msg_opt);

        if let Some(sequence_id) = message.sequence_id() {
            // If the message is a push status, make the sequence id "status".
            if let Message::Print(msg) = &message {
                if msg.command == PrintCommand::PushStatus {
                    self.responses.insert(SequenceId::status(), message);
                    return Ok(());
                }
            }
            self.responses.insert(sequence_id, message);
            return Ok(());
        }

        if let Message::Unknown(None) = message {
            return Ok(());
        }

        tracing::error!("Received message AND COULD NOT INSERT: {:?}", message);

        Ok(())
    }

    /// Get the latest status of the printer.
    pub fn get_status(&self) -> Result<Option<Print>> {
        let response = self.responses.get(&SequenceId::status());
        if let Some(response) = response {
            if let Message::Print(print) = response.value() {
                return Ok(Some(print.clone()));
            }
        }

        Ok(None)
    }

    async fn subscribe_to_device_report(&self) -> Result<()> {
        self.client
            .subscribe(&self.topic_device_report, rumqttc::mqttbytes::QoS::AtMostOnce)
            .await?;

        Ok(())
    }

    /// Runs the Bambu MQTT client.
    /// You should run this in a tokio task.
    ///
    /// # Errors
    ///
    /// Returns an error if there was a problem connecting to the MQTT broker
    /// or subscribing to the device report topic.
    pub async fn run(&mut self) -> Result<()> {
        self.subscribe_to_device_report().await?;

        loop {
            Self::poll(self).await?;
        }
    }

    /// Publishes a command to the Bambu MQTT broker.
    ///
    /// # Errors
    ///
    /// Returns an error if there was a problem publishing the command.
    pub async fn publish(&self, command: Command) -> Result<Message> {
        let sequence_id = command.sequence_id();
        let payload = serde_json::to_string(&command)?;

        self.client
            .publish(
                &self.topic_device_request,
                rumqttc::mqttbytes::QoS::AtMostOnce,
                false,
                payload,
            )
            .await?;

        // Wait for the response.
        let current_time = std::time::Instant::now();
        while current_time.elapsed().as_secs() < 60 {
            if let Some(response) = self.responses.get(sequence_id) {
                return Ok(response.value().clone());
            }
        }

        anyhow::bail!("Timeout waiting for response to command: {:?}", command)
    }

    /// Upload a file.
    pub async fn upload_file(&self, path: &std::path::Path) -> Result<()> {
        let host_url = url::Url::parse(&self.host)?;
        let host = host_url
            .host_str()
            .ok_or(anyhow::anyhow!("not a valid hostname"))?
            .to_string();
        let access_code = self.access_code.clone();
        let path = path.to_path_buf();
        let args: Vec<String> = vec![
            "--silent".to_string(),
            "--upload-file".to_string(),
            path.to_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid file path"))?
                .to_string(),
            "--ftp-pasv".to_string(),
            "--insecure".to_string(),
            format!("ftps://{}/", host).to_string(),
            "--user".to_string(),
            format!("bblp:{}", access_code).to_string(),
        ];
        let output = tokio::process::Command::new("curl")
            .args(&args)
            .output()
            .await
            .context("Failed to upload file")?;

        // Make sure the command was successful.
        if !output.status.success() {
            let stdout = std::str::from_utf8(&output.stdout)?;
            let stderr = std::str::from_utf8(&output.stderr)?;
            anyhow::bail!(
                "Failed to upload file: {:?}\nstdout:\n{}stderr:{}",
                output,
                stdout,
                stderr
            );
        }

        Ok(())
    }
}
