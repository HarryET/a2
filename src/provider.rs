use solicit::http::client::tls::TlsConnector;
use solicit::client::{Client};
use solicit::http::{Header, Response as HttpResponse};
use notification::*;
use response::*;
use openssl::ssl::{SslContext, SslMethod, SSL_VERIFY_NONE};
use openssl::x509::X509;
use openssl::crypto::pkey::PKey;
use time::{Tm, Timespec, at, precise_time_ns};
use rustc_serialize::json::{Json, Object};
use std::str;
use std::time::{Instant, Duration};
use std::fmt::Display;
use std::result::Result;
use std::thread;
use std::fs::File;
use std::io::Read;
use std::sync::mpsc::Receiver;

static DEVELOPMENT: &'static str = "api.development.push.apple.com";
static PRODUCTION: &'static str = "api.push.apple.com";

pub struct Provider {
    pub client: Client,
}

impl Provider {
    pub fn new(sandbox: bool, certificate: &str, private_key: &str) -> Provider {
        Provider::from_reader(sandbox,
                              &mut File::open(certificate).unwrap(),
                              &mut File::open(private_key).unwrap())
    }

    pub fn from_reader<R: Read>(sandbox: bool, certificate: &mut R, private_key: &mut R) -> Provider {
        let host    = if sandbox { DEVELOPMENT } else { PRODUCTION };
        let mut ctx = SslContext::new(SslMethod::Sslv23).unwrap();

        let x509 = X509::from_pem(certificate).unwrap();
        let pkey = PKey::private_key_from_pem(private_key).unwrap();

        ctx.set_cipher_list("DEFAULT").unwrap();
        ctx.set_certificate(&x509).unwrap();
        ctx.set_private_key(&pkey).unwrap();
        ctx.set_verify(SSL_VERIFY_NONE, None);
        ctx.set_alpn_protocols(&[b"h2"]);

        let connector = TlsConnector::with_context(host, &ctx);
        let client    = Client::with_connector(connector).unwrap();

        Provider { client: client, }
    }

    pub fn push(&self, notification: Notification) -> AsyncResponse {
        AsyncResponse::new(self.request(notification), precise_time_ns())
    }

    fn request(&self, notification: Notification) -> Option<Receiver<HttpResponse<'static, 'static>>> {
        let path = format!("/3/device/{}", notification.device_token).into_bytes();
        let body = notification.payload.to_string().into_bytes();

        let mut headers = Vec::new();
        headers.push(Provider::create_header("content_length", notification.payload.len()));

        if let Some(apns_id) = notification.options.apns_id {
            headers.push(Provider::create_header("apns-id", apns_id));
        }

        if let Some(apns_expiration) = notification.options.apns_expiration {
            headers.push(Provider::create_header("apns-expiration", apns_expiration));
        }

        if let Some(apns_priority) = notification.options.apns_priority {
            headers.push(Provider::create_header("apns-priority", apns_priority));
        }

        if let Some(apns_topic) = notification.options.apns_topic {
            headers.push(Provider::create_header("apns-topic", apns_topic));
        }

        self.client.post(&path, headers.as_slice(), body)
    }

    fn create_header<'a, T: Display>(key: &'a str, value: T) -> Header<'a, 'a> {
        Header::new(key.as_bytes(), format!("{}", value).into_bytes())
    }

}

pub type ResponseChannel = Receiver<HttpResponse<'static, 'static>>;

pub struct AsyncResponse {
    rx: Option<ResponseChannel>,
    pub requested_at: u64,
}

impl AsyncResponse {
    pub fn new(rx: Option<ResponseChannel>, requested_at: u64) -> AsyncResponse {
        AsyncResponse { rx: rx, requested_at: requested_at }
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Response, Response> {
        if let Some(ref rx) = self.rx {
            let now = Instant::now();

            while now.elapsed() < timeout {
                match rx.try_recv() {
                    Ok(http_response) => {
                        let status        = AsyncResponse::fetch_status(http_response.status_code().ok());
                        let apns_id       = AsyncResponse::fetch_apns_id(http_response.headers);
                        let json          = str::from_utf8(&http_response.body).ok().and_then(|v| Json::from_str(v).ok());
                        let object        = json.as_ref().and_then(|v| v.as_object());
                        let reason        = AsyncResponse::fetch_reason(object);
                        let timestamp     = AsyncResponse::fetch_timestamp(object);

                        let response = Response {
                            status: status,
                            reason: reason,
                            timestamp: timestamp,
                            apns_id: apns_id,
                        };

                        if response.status == APNSStatus::Success {
                            return Ok(response);
                        } else {
                            return Err(response);
                        }
                    },
                    _ => thread::park_timeout(Duration::from_millis(10)),
                }
            }

            Err(Response {
                status: APNSStatus::Timeout,
                reason: None,
                timestamp: None,
                apns_id: None,
            })
        } else {
            Err(Response {
                status: APNSStatus::MissingChannel,
                reason: None,
                timestamp: None,
                apns_id: None,
            })
        }
    }

    fn fetch_status(code: Option<u16>) -> APNSStatus {
        match code {
            Some(200) => APNSStatus::Success,
            Some(400) => APNSStatus::BadRequest,
            Some(403) => APNSStatus::Forbidden,
            Some(405) => APNSStatus::MethodNotAllowed,
            Some(410) => APNSStatus::Unregistered,
            Some(413) => APNSStatus::PayloadTooLarge,
            Some(429) => APNSStatus::TooManyRequests,
            Some(500) => APNSStatus::InternalServerError,
            Some(503) => APNSStatus::ServiceUnavailable,
            _         => APNSStatus::Unknown,
        }
    }

    fn fetch_apns_id(headers: Vec<Header>) -> Option<String> {
        headers.iter().find(|&header| {
            match str::from_utf8(header.name()).unwrap() {
                "apns-id" => true,
                _         => false,
            }
        }).map(|header| {
            String::from_utf8(header.value().to_vec()).unwrap()
        })
    }

    fn fetch_reason(js_object: Option<&Object>) -> Option<APNSError> {
        let raw_reason = js_object.and_then(|v| v.get("reason")).and_then(|v| v.as_string());

        match raw_reason {
            Some("PayloadEmpty")              => Some(APNSError::PayloadEmpty),
            Some("PayloadTooLarge")           => Some(APNSError::PayloadTooLarge),
            Some("BadTopic")                  => Some(APNSError::BadTopic),
            Some("TopicDisallowed")           => Some(APNSError::TopicDisallowed),
            Some("BadMessageId")              => Some(APNSError::BadMessageId),
            Some("BadExpirationDate")         => Some(APNSError::BadExpirationDate),
            Some("BadPriority")               => Some(APNSError::BadPriority),
            Some("MissingDeviceToken")        => Some(APNSError::MissingDeviceToken),
            Some("BadDeviceToken")            => Some(APNSError::BadDeviceToken),
            Some("DeviceTokenNotForTopic")    => Some(APNSError::DeviceTokenNotForTopic),
            Some("Unresgistered")             => Some(APNSError::Unregistered),
            Some("DuplicateHeaders")          => Some(APNSError::DuplicateHeaders),
            Some("BadCertificateEnvironment") => Some(APNSError::BadCertificateEnvironment),
            Some("BadCertificate")            => Some(APNSError::BadCertificate),
            Some("Forbidden")                 => Some(APNSError::Forbidden),
            Some("BadPath")                   => Some(APNSError::BadPath),
            Some("MethodNotAllowed")          => Some(APNSError::MethodNotAllowed),
            Some("TooManyRequests")           => Some(APNSError::TooManyRequests),
            Some("IdleTimeout")               => Some(APNSError::IdleTimeout),
            Some("Shutdown")                  => Some(APNSError::Shutdown),
            Some("InternalServerError")       => Some(APNSError::InternalServerError),
            Some("ServiceUnavailable")        => Some(APNSError::ServiceUnavailable),
            Some("MissingTopic")              => Some(APNSError::MissingTopic),
            _                                 => None,
        }
    }

    fn fetch_timestamp(js_object: Option<&Object>) -> Option<Tm> {
        let raw_ts = js_object.and_then(|v| v.get("timestamp")).and_then(|v| v.as_i64());

        match raw_ts {
            Some(ts) => Some(at(Timespec::new(ts, 0))),
            None     => None,
        }
    }
}
