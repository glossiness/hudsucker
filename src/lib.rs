mod certificate_authority;
mod error;

use certificate_authority::CertificateAuthority;
use error::Error;
use hyper::client::HttpConnector;
use hyper::server::conn::Http;
use hyper::service::{make_service_fn, service_fn};
use hyper::upgrade::Upgraded;
use hyper::{Body, Client, Method, Request, Response, Server};
use hyper_tls::HttpsConnector;
use rcgen::RcgenError;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

type HttpClient = Client<HttpsConnector<HttpConnector>>;

pub type RequestHandler = fn(Request<Body>) -> (Request<Body>, Option<Response<Body>>);
pub type ResponseHandler = fn(Response<Body>) -> Response<Body>;

pub struct ProxyConfig<F: Future<Output = ()>> {
    pub listen_addr: SocketAddr,
    pub shutdown_signal: F,
    pub request_handler: Option<RequestHandler>,
    pub response_handler: Option<ResponseHandler>,
    pub private_key: rustls::PrivateKey,
}

pub async fn start_proxy<F>(
    ProxyConfig {
        listen_addr,
        shutdown_signal,
        request_handler,
        response_handler,
        private_key,
    }: ProxyConfig<F>,
) -> Result<(), Error>
where
    F: Future<Output = ()>,
{
    validate_key(&private_key)?;

    let client = Client::builder()
        .http1_title_case_headers(true)
        .http1_preserve_header_case(true)
        .build(HttpsConnector::new());

    let ca = CertificateAuthority::new(private_key);
    let request_handler = request_handler.unwrap_or(|req| (req, None));
    let response_handler = response_handler.unwrap_or(|res| res);

    let make_service = make_service_fn(move |_| {
        let client = client.clone();
        let ca = ca.clone();
        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                proxy(
                    req,
                    client.clone(),
                    ca.clone(),
                    request_handler,
                    response_handler,
                )
            }))
        }
    });

    Server::bind(&listen_addr)
        .http1_preserve_header_case(true)
        .http1_title_case_headers(true)
        .serve(make_service)
        .with_graceful_shutdown(shutdown_signal)
        .await?;

    Ok(())
}

async fn proxy(
    req: Request<Body>,
    client: HttpClient,
    ca: CertificateAuthority,
    handle_req: RequestHandler,
    handle_res: ResponseHandler,
) -> Result<Response<Body>, hyper::Error> {
    if req.method() == Method::CONNECT {
        process_connect(req, client, ca, handle_req, handle_res)
    } else {
        process_request(req, client, handle_req, handle_res).await
    }
}

async fn process_request(
    req: Request<Body>,
    client: HttpClient,
    handle_req: RequestHandler,
    handle_res: ResponseHandler,
) -> Result<Response<Body>, hyper::Error> {
    let (req, res) = handle_req(req);
    if let Some(res) = res {
        return Ok(res);
    }

    let res = client.request(req).await?;
    Ok(handle_res(res))
}

fn process_connect(
    req: Request<Body>,
    client: HttpClient,
    ca: CertificateAuthority,
    handle_req: RequestHandler,
    handle_res: ResponseHandler,
) -> Result<Response<Body>, hyper::Error> {
    let authority = req.uri().authority().unwrap();
    let server_config = Arc::new(ca.gen_server_config(authority));

    tokio::task::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                // TODO: handle Err
                let stream = TlsAcceptor::from(server_config)
                    .accept(upgraded)
                    .await
                    .unwrap();
                serve_connection(stream, client, handle_req, handle_res).await;
            }
            Err(e) => eprintln!("upgrade error: {}", e),
        };
    });

    Ok(Response::new(Body::empty()))
}

async fn serve_connection(
    stream: tokio_rustls::server::TlsStream<Upgraded>,
    client: HttpClient,
    handle_req: RequestHandler,
    handle_res: ResponseHandler,
) -> Result<(), hyper::Error> {
    let service = service_fn(|req| {
        let authority = req.headers().get("host").unwrap().to_str().unwrap();
        let uri = http::uri::Builder::new()
            .scheme("https")
            .authority(authority)
            .path_and_query(req.uri().to_string())
            .build()
            .unwrap();
        let (mut parts, body) = req.into_parts();
        parts.uri = uri;
        let req = Request::from_parts(parts, body);
        process_request(req, client.clone(), handle_req, handle_res)
    });
    Http::new().serve_connection(stream, service).await
}

pub fn validate_key(key_pair: &rustls::PrivateKey) -> Result<(), RcgenError> {
    rcgen::KeyPair::from_der(&key_pair.0)?;
    Ok(())
}
