use std::error;
use std::io;
use std::io::prelude::*;
use std::sync::{Arc, Mutex, Once};
use url::Url;

use log::{debug, info};

use git2::transport::{Service, SmartSubtransport, SmartSubtransportStream, Transport};
use git2::Error;

#[derive(Default)]
struct UreqTransport {
    /// The URL of the remote server, e.g. "https://github.com/user/repo"
    ///
    /// This is an empty string until the first action is performed.
    /// If there is an HTTP redirect, this will be updated with the new URL.
    base_url: Arc<Mutex<String>>,
}

struct UreqSubtransport {
    service: &'static str,
    url_path: &'static str,
    base_url: Arc<Mutex<String>>,
    method: &'static str,
    reader: Option<Box<dyn Read + Send>>,
    sent_request: bool,
}

pub unsafe fn register() {
    static INIT: Once = Once::new();

    INIT.call_once(move || {
        git2::transport::register("http", move |remote| factory(remote)).unwrap();
        git2::transport::register("https", move |remote| factory(remote)).unwrap();
    });
}

fn factory(remote: &git2::Remote<'_>) -> Result<Transport, Error> {
    Transport::smart(remote, true, UreqTransport::default())
}

impl SmartSubtransport for UreqTransport {
    fn action(
        &self,
        url: &str,
        action: Service,
    ) -> Result<Box<dyn SmartSubtransportStream>, Error> {
        let mut base_url = self.base_url.lock().unwrap();
        if base_url.len() == 0 {
            *base_url = url.to_string();
        }
        let (service, path, method) = match action {
            Service::UploadPackLs => ("upload-pack", "/info/refs?service=git-upload-pack", "GET"),
            Service::UploadPack => ("upload-pack", "/git-upload-pack", "POST"),
            Service::ReceivePackLs => {
                ("receive-pack", "/info/refs?service=git-receive-pack", "GET")
            }
            Service::ReceivePack => ("receive-pack", "/git-receive-pack", "POST"),
        };
        info!("action {} {}", service, path);
        Ok(Box::new(UreqSubtransport {
            service,
            url_path: path,
            base_url: self.base_url.clone(),
            method,
            reader: None,
            sent_request: false,
        }))
    }

    fn close(&self) -> Result<(), Error> {
        Ok(())
    }
}

impl UreqSubtransport {
    fn err<E: Into<Box<dyn error::Error + Send + Sync>>>(&self, err: E) -> io::Error {
        io::Error::new(io::ErrorKind::Other, err)
    }

    fn execute(&mut self, data: &[u8]) -> io::Result<()> {
        if self.sent_request {
            return Err(self.err("already sent HTTP request"));
        }

        let agent = format!("git/1.0 (git2-ureq {})", env!("CARGO_PKG_VERSION"));

        // Parse our input URL to figure out the host
        let url = format!("{}{}", self.base_url.lock().unwrap(), self.url_path);
        let parsed = Url::parse(&url).map_err(|_| self.err("invalid url, failed to parse"))?;
        let host = match parsed.host_str() {
            Some(host) => host,
            None => return Err(self.err("invalid url, did not have a host")),
        };

        // Prep the request
        debug!("request to {}", url);
        let request = ureq::request(self.method, &url)
            .set("User-Agent", &agent)
            .set("Host", &host)
            .set("Expect", "");
        let request = if data.is_empty() {
            request.set("Accept", "*/*")
        } else {
            request
                .set(
                    "Accept",
                    &format!("application/x-git-{}-result", self.service),
                )
                .set(
                    "Conent-Type",
                    &format!("application/x-git-{}-request", self.service),
                )
        };

        let response = request.send(data).unwrap();
        let content_type = response.header("Content-Type");

        let code = response.status();
        if code != 200 {
            return Err(self.err(&format!("failed to receive HTTP 200 response: got {code}",)[..]));
        }

        // Check returned headers
        let expected = match self.method {
            "GET" => format!("application/x-git-{}-advertisement", self.service),
            _ => format!("application/x-git-{}-result", self.service),
        };

        if let Some(content_type) = content_type {
            if content_type != expected {
                return Err(self.err(
                    &format!(
                        "expected a Content-Type header with `{expected}` but found `{content_type}`",
                    )[..],
                ));
            }
        } else {
            return Err(
                self.err(
                    &format!(
                        "expected a Content-Type header with `{expected}` but didn't find one"
                    )[..],
                ),
            );
        }

        // preserve response body for reading afterwards
        self.reader = Some(Box::new(response.into_reader()));

        Ok(())
    }
}

impl Read for UreqSubtransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.reader.is_none() {
            self.execute(&[])?;
        }

        self.reader.as_mut().unwrap().read(buf)
    }
}

impl Write for UreqSubtransport {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.reader.is_none() {
            self.execute(data)?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
