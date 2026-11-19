//! The HTTP client for one board.
//!
//! It talks only to the configured board URL: no proxy from the environment,
//! no redirects, no other hosts. M0 boards are plain HTTP on loopback.

use std::time::Duration;

use openboard_api::{
    codes as api_codes, ChannelList, ErrorBody, EventsQuery, EventsResponse, PostMessageRequest,
    PostMessageResponse, Thread, ThreadList, ThreadsQuery, PROFILE_HEADER,
};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::{Method, RequestBuilder, Response, Url};
use serde::de::DeserializeOwned;

use crate::error::{codes, Failure};

/// How long to wait for a TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an ordinary request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Extra time allowed on top of a long-poll's own wait.
const LONG_POLL_GRACE: Duration = Duration::from_secs(30);

pub struct Client {
    http: reqwest::Client,
    base: Url,
    board: String,
    profile: Option<String>,
}

impl Client {
    pub fn new(url: &str, board: &str, profile: Option<&str>) -> Result<Self, Failure> {
        let base = Url::parse(url).map_err(|error| {
            Failure::usage(format!("the board URL {url:?} is not a valid URL: {error}"))
                .next("set --url or BOARD_URL to something like http://127.0.0.1:7878")
        })?;
        if base.scheme() != "http" || base.host_str().is_none() {
            return Err(Failure::usage(format!(
                "the board URL {url:?} must start with http://; this version only talks to a \
                 local board over plain HTTP"
            ))
            .next("set --url or BOARD_URL to something like http://127.0.0.1:7878"));
        }
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("openboard/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| {
                Failure::new(
                    codes::REQUEST_FAILED,
                    format!("cannot set up the HTTP client: {error}"),
                )
            })?;
        Ok(Self {
            http,
            base,
            board: board.to_owned(),
            profile: profile.map(str::to_owned),
        })
    }

    /// `<base>/v1/boards/<board>/<rest...>`, with every segment escaped.
    fn endpoint(&self, rest: &[&str]) -> Url {
        let mut url = self.base.clone();
        url.set_query(None);
        url.set_fragment(None);
        if let Ok(mut segments) = url.path_segments_mut() {
            segments
                .pop_if_empty()
                .extend(["v1", "boards", self.board.as_str()])
                .extend(rest);
        }
        url
    }

    fn request(&self, method: Method, url: Url, timeout: Option<Duration>) -> RequestBuilder {
        let mut request = self.http.request(method, url);
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        // An empty profile header is an error, so it is only sent when set.
        if let Some(profile) = &self.profile {
            request = request.header(PROFILE_HEADER, profile);
        }
        request
    }

    async fn send(&self, request: RequestBuilder) -> Result<Response, Failure> {
        let response = request
            .send()
            .await
            .map_err(|error| self.transport(&error))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|error| self.transport(&error))?;
        Err(match serde_json::from_slice::<ErrorBody>(&body) {
            Ok(body) => self.api_failure(body),
            Err(_) => Failure::new(
                codes::BAD_RESPONSE,
                format!(
                    "{} answered HTTP {status} without an Openboard error body",
                    self.base
                ),
            )
            .next("check that --url or BOARD_URL points at an Openboard server"),
        })
    }

    async fn json<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T, Failure> {
        let body = self
            .send(request)
            .await?
            .bytes()
            .await
            .map_err(|error| self.transport(&error))?;
        serde_json::from_slice(&body).map_err(|error| {
            Failure::new(
                codes::BAD_RESPONSE,
                format!(
                    "{} sent a response this client cannot read: {error}",
                    self.base
                ),
            )
            .next("check that --url or BOARD_URL points at an Openboard server of this version")
        })
    }

    /// Turns a server error into a failure, replacing API-level hints with
    /// the CLI command that does the same thing.
    fn api_failure(&self, body: ErrorBody) -> Failure {
        let error = body.error;
        let next = match error.code.as_str() {
            api_codes::INVALID_CURSOR => Some(
                "use a cursor printed by this board; omit --after to start from now, or run \
                 `openboard tail --from-start` to see every event"
                    .to_owned(),
            ),
            api_codes::INVALID_PROFILE => {
                Some("fix or drop --as (or BOARD_AS), for example `--as agent-a`".to_owned())
            }
            // The usual cause is a wrong thread ID. Requests that name no
            // thread replace this hint; see `board_missing`.
            api_codes::NOT_FOUND => Some(
                "run `openboard channels`, then `openboard read <channel>` to find thread IDs"
                    .to_owned(),
            ),
            api_codes::INVALID_CHANNEL => Some(
                "use a path like `research/web`: lowercase letters, digits, '_' and '-' joined \
                 by '/'"
                    .to_owned(),
            ),
            api_codes::IDEMPOTENCY_CONFLICT => Some("use a new --key for a new message".to_owned()),
            _ => error.next,
        };
        let mut failure = Failure::new(error.code, error.message);
        failure.next = next;
        failure
    }

    /// Rewrites the hint of a `not_found` answer to a request that names no
    /// thread. There the only thing that can be missing is the board, and a
    /// board exists only once something has been posted to it.
    fn board_missing(&self, failure: Failure) -> Failure {
        if failure.code != api_codes::NOT_FOUND {
            return failure;
        }
        failure.next(format!(
            "board `{}` does not exist yet: the first `openboard post` to it creates it; if \
             you meant another board, check --board (or BOARD_NAME)",
            self.board
        ))
    }

    fn transport(&self, error: &reqwest::Error) -> Failure {
        // The innermost cause is the useful part, e.g. "Connection refused".
        let mut cause: &dyn std::error::Error = error;
        while let Some(inner) = cause.source() {
            cause = inner;
        }
        let base = &self.base;
        if error.is_connect() {
            Failure::new(
                codes::CONNECTION_FAILED,
                format!("cannot reach a board at {base}: {cause}"),
            )
            .next(
                "start a board with `openboard serve`, or point --url (or BOARD_URL) at a \
                 running board",
            )
        } else if error.is_timeout() {
            Failure::new(
                codes::REQUEST_TIMEOUT,
                format!("the board at {base} did not answer in time"),
            )
            .next("check that the board is running, then retry")
        } else if error.is_builder() {
            Failure::usage(format!("cannot build the request: {cause}"))
                .next("check --url, --board and --as for stray characters")
        } else {
            Failure::new(
                codes::REQUEST_FAILED,
                format!("the request to {base} failed: {cause}"),
            )
            .next("check that the board is running, then retry")
        }
    }

    pub async fn post_message(
        &self,
        request: &PostMessageRequest,
    ) -> Result<PostMessageResponse, Failure> {
        let body = serde_json::to_vec(request)
            .map_err(|error| Failure::new(codes::IO, format!("cannot encode the post: {error}")))?;
        let request = self
            .request(
                Method::POST,
                self.endpoint(&["messages"]),
                Some(REQUEST_TIMEOUT),
            )
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        self.json(request).await
    }

    pub async fn channels(&self) -> Result<ChannelList, Failure> {
        self.json(self.request(
            Method::GET,
            self.endpoint(&["channels"]),
            Some(REQUEST_TIMEOUT),
        ))
        .await
        .map_err(|failure| self.board_missing(failure))
    }

    pub async fn threads(&self, query: &ThreadsQuery) -> Result<ThreadList, Failure> {
        let mut url = self.endpoint(&["threads"]);
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("channel", &query.channel);
            if let Some(limit) = query.limit {
                pairs.append_pair("limit", &limit.to_string());
            }
            if let Some(cursor) = &query.cursor {
                pairs.append_pair("cursor", cursor);
            }
        }
        self.json(self.request(Method::GET, url, Some(REQUEST_TIMEOUT)))
            .await
            .map_err(|failure| self.board_missing(failure))
    }

    /// The thread that holds `id`, which may be any message ID in it.
    pub async fn thread(&self, id: &str) -> Result<Thread, Failure> {
        self.json(self.request(
            Method::GET,
            self.endpoint(&["threads", id]),
            Some(REQUEST_TIMEOUT),
        ))
        .await
    }

    fn events_url(&self, rest: &[&str], query: &EventsQuery) -> Url {
        let mut url = self.endpoint(rest);
        {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in [
                ("after", &query.after),
                ("channel", &query.channel),
                ("thread", &query.thread),
                ("not_from", &query.not_from),
            ] {
                if let Some(value) = value {
                    pairs.append_pair(name, value);
                }
            }
            if let Some(wait) = query.wait {
                pairs.append_pair("wait", &wait.to_string());
            }
            if let Some(limit) = query.limit {
                pairs.append_pair("limit", &limit.to_string());
            }
        }
        if url.query() == Some("") {
            url.set_query(None);
        }
        url
    }

    /// One events request. With `query.wait` set this is a long-poll.
    pub async fn events(&self, query: &EventsQuery) -> Result<EventsResponse, Failure> {
        let timeout = Duration::from_secs(query.wait.unwrap_or(0)) + LONG_POLL_GRACE;
        self.json(self.request(
            Method::GET,
            self.events_url(&["events"], query),
            Some(timeout),
        ))
        .await
        .map_err(|failure| self.events_not_found(query, failure))
    }

    /// With a thread filter, `not_found` most likely means the thread.
    fn events_not_found(&self, query: &EventsQuery, failure: Failure) -> Failure {
        match query.thread {
            Some(_) => failure,
            None => self.board_missing(failure),
        }
    }

    /// Opens the server-sent event stream. The response has no overall
    /// timeout; the caller watches for silence.
    pub async fn event_stream(&self, query: &EventsQuery) -> Result<Response, Failure> {
        let request = self
            .request(
                Method::GET,
                self.events_url(&["events", "stream"], query),
                None,
            )
            .header(ACCEPT, "text/event-stream");
        self.send(request)
            .await
            .map_err(|failure| self.events_not_found(query, failure))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_escape_every_segment() {
        let client = Client::new("http://127.0.0.1:7878/", "Team Board", None).unwrap();
        assert_eq!(
            client.endpoint(&["threads", "a/b?c"]).as_str(),
            "http://127.0.0.1:7878/v1/boards/Team%20Board/threads/a%2Fb%3Fc"
        );
        let query = EventsQuery {
            after: Some("ev_1".into()),
            channel: Some("research/".into()),
            not_from: Some("local/agent-a".into()),
            wait: Some(60),
            ..EventsQuery::default()
        };
        assert_eq!(
            client.events_url(&["events"], &query).as_str(),
            "http://127.0.0.1:7878/v1/boards/Team%20Board/events?after=ev_1&channel=research%2F&not_from=local%2Fagent-a&wait=60"
        );
        assert_eq!(
            client
                .events_url(&["events", "stream"], &EventsQuery::default())
                .as_str(),
            "http://127.0.0.1:7878/v1/boards/Team%20Board/events/stream"
        );
    }

    #[test]
    fn only_plain_http_urls_are_accepted() {
        assert!(Client::new("http://localhost:7878", "general", None).is_ok());
        for bad in ["https://127.0.0.1:7878", "127.0.0.1:7878", "", "ftp://x"] {
            let failure = Client::new(bad, "general", None).err().expect(bad);
            assert_eq!(failure.exit, crate::error::EXIT_USAGE, "{bad}");
        }
    }
}
