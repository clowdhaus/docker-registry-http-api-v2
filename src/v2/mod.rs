//! Docker Registry API v2.

use hyper::{self, client};
use hyper_tls;
use tokio_core::reactor;
use super::errors::*;
use futures;
use serde_json;

use std::str::FromStr;
use futures::{Future, Stream};

mod config;
pub use self::config::Config;

mod manifest;
pub use self::manifest::{Manifest, FutureManifest};

mod tags;
pub use self::tags::{Tags, FutureTags};

mod blobs;
pub use self::blobs::FutureUuid;

/// A Client to make outgoing API requests to a registry.
#[derive(Debug)]
pub struct Client {
    base_url: String,
    credentials: Option<(String, String)>,
    hclient: client::Client<hyper_tls::HttpsConnector>,
    index: String,
    user_agent: Option<String>,
    token: Option<String>,
}

pub type FutureBool = Box<futures::Future<Item = bool, Error = Error>>;

impl Client {
    pub fn configure(handle: &reactor::Handle) -> Config {
        Config::default(handle)
    }

    fn new_request(&self, method: hyper::Method, url: hyper::Uri) -> hyper::client::Request {
        let mut req = client::Request::new(method, url);
        if let Some(ref t) = self.token {
            req.headers_mut().set(hyper::header::Authorization(hyper::header::Bearer {
                                                                   token: t.to_owned(),
                                                               }));
        };
        if let Some(ref ua) = self.user_agent {
            req.headers_mut().set(hyper::header::UserAgent(ua.to_owned()));
        };
        return req;
    }

    pub fn is_v2_supported(&self) -> Result<FutureBool> {
        let api_header = "Docker-Distribution-API-Version";
        let api_version = "registry/2.0";

        let url = try!(hyper::Uri::from_str((self.base_url.clone() + "/v2/").as_str()));
        let req = self.new_request(hyper::Method::Get, url);
        let freq = self.hclient.request(req);
        let fres =
            freq.and_then(move |r| match (r.status(), r.headers().get_raw(api_header)) {
                              (&hyper::status::StatusCode::Ok, Some(x)) => Ok(x == api_version),
                              (&hyper::status::StatusCode::Unauthorized, Some(x)) => {
                                  Ok(x == api_version)
                              }
                              (_, _) => Ok(false),
                          })
                .map_err(|e| e.into());
        return Ok(Box::new(fres));
    }

    pub fn is_auth(&self) -> Result<FutureBool> {
        let url = try!(hyper::Uri::from_str((self.base_url.clone() + "/v2/").as_str()));
        let req = self.new_request(hyper::Method::Get, url);
        let freq = self.hclient.request(req);
        let fres = freq.and_then(move |r| match r.status() {
                                     &hyper::status::StatusCode::Ok => Ok(true),
                                     _ => Ok(false),
                                 })
            .map_err(|e| e.into());
        return Ok(Box::new(fres));
    }

    pub fn login(&mut self, scopes: Vec<&str>) -> Result<()> {
        let mut tcore = try!(reactor::Core::new());
        let client = hyper::client::Client::configure()
            .connector(hyper_tls::HttpsConnector::new(4, &tcore.handle()))
            .build(&tcore.handle());
        let url = try!(hyper::Uri::from_str((self.base_url.clone() + "/v2/").as_str()));
        let req = self.new_request(hyper::Method::Get, url);
        let resp = tcore.run(client.request(req))?;
        let www_auth = resp.headers()
            .get_raw("www-authenticate")
            .ok_or("missing header")?
            .one()
            .ok_or("multiple headers")?;
        match resp.status() {
            &hyper::status::StatusCode::Ok => return Ok(()),
            &hyper::status::StatusCode::Unauthorized => {}
            _ => return Err("unexpected status".into()),
        };
        let chal = try!(String::from_utf8(www_auth.to_vec()));
        let mut auth_ep = "".to_owned();
        let mut service = None;
        for item in chal.trim_left_matches("Bearer ").split(',') {
            let kv: Vec<&str> = item.split('=').collect();
            match (kv.get(0), kv.get(1)) {
                (Some(&"realm"), Some(v)) => auth_ep = v.trim_matches('"').to_owned(),
                (Some(&"service"), Some(v)) => service = Some(v.trim_matches('"').clone()),
                (Some(&"scope"), _) => {}
                (_, _) => return Err("unsupported key".into()),
            };
        }

        if let Some(sv) = service {
            auth_ep += &format!("?service={}", sv);
        }
        for sc in scopes {
            auth_ep += &format!("&scope={}", sc);
        }
        let auth_url = try!(hyper::Uri::from_str(auth_ep.as_str()));
        trace!("Token endpoint: \"{}\"", auth_url);

        let mut auth_req = client::Request::new(hyper::Method::Get, auth_url);
        if let Some(ref creds) = self.credentials {
            auth_req.headers_mut()
                .set(hyper::header::Authorization(hyper::header::Basic {
                                                      username: creds.0.to_owned(),
                                                      password: Some(creds.1.to_owned()),
                                                  }))
        };
        let fut_req = client.request(auth_req);
        let auth_resp = fut_req.map_err(|e| e.into())
            .and_then(move |r| {
                          if r.status() != &hyper::status::StatusCode::Ok {
                              return Err(hyper::Error::Status);
                          };
                          Ok(r)
                      })
            .and_then(move |r| {
                          r.body().fold(Vec::new(), |mut v, chunk| {
                    v.extend(&chunk[..]);
                    futures::future::ok::<_, hyper::Error>(v)
                })
                      })
            .and_then(|chunks| {
                          let s = String::from_utf8(chunks).unwrap();
                          Ok(s)
                      })
            .map_err(|e| e.into())
            .and_then(move |body| -> Result<TokenAuth> {
                          serde_json::from_slice(body.as_bytes()).map_err(|e| e.into())
                      });

        let t: TokenAuth = tcore.run(auth_resp)?;
        self.token = Some(t.token);
        Ok(())
    }

    pub fn get_catalog(&self, limit: Option<u32>) -> Result<FutureCatalog> {
        let url = {
            let mut s = self.base_url.clone() + "/v2/_catalog";
            if let Some(n) = limit {
                s = s + &format!("?n={}", n);
            };
            try!(hyper::Uri::from_str(s.as_str()))
        };
        let req = self.new_request(hyper::Method::Get, url);
        let freq = self.hclient.request(req);
        let fres = freq.map_err(|e| e.into())
            .and_then(move |r| {
                          if r.status() != &hyper::status::StatusCode::Ok {
                              return Err(hyper::Error::Status);
                          };
                          Ok(r)
                      })
            .and_then(move |r| {
                          r.body().fold(Vec::new(), |mut v, chunk| {
                    v.extend(&chunk[..]);
                    futures::future::ok::<_, hyper::Error>(v)
                })
                      })
            .and_then(|chunks| {
                          let s = String::from_utf8(chunks).unwrap();
                          Ok(s)
                      })
            .and_then(move |body| {
                          serde_json::from_slice(body.as_bytes()).map_err(|_| hyper::Error::Status)
                      })
            .map_err(|e| e.into());
        return Ok(Box::new(fres));
    }
}

pub type FutureToken = Box<futures::Future<Item = TokenAuth, Error = Error>>;
#[derive(Debug,Default,Deserialize,Serialize)]
pub struct TokenAuth {
    pub token: String,
    pub expires_in: Option<u32>,
    pub issued_at: Option<String>,
    pub refresh_token: Option<String>,
}

pub type FutureCatalog = Box<futures::Future<Item = Catalog, Error = Error>>;
#[derive(Debug,Default,Deserialize,Serialize)]
pub struct Catalog {
    pub repositories: Vec<String>,
}

#[derive(Debug,Default,Deserialize,Serialize)]
pub struct ApiError {
    code: String,
    message: String,
    detail: String,
}

#[derive(Debug,Default,Deserialize,Serialize)]
pub struct Errors {
    errors: Vec<ApiError>,
}
