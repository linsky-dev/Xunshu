//! Bounded JSON-lines protocol over inherited stdin/stdout, no network listener.
use crate::{operations, Index, SearchOptions};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    io::{BufRead, Write},
    path::Path,
};
const MAX_REQUEST: usize = 64 * 1024;
#[derive(Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Status {
        id: Value,
    },
    Search {
        id: Value,
        query: String,
        #[serde(default)]
        options: SearchOptions,
    },
    Reload {
        id: Value,
    },
    Shutdown {
        id: Value,
    },
}
impl Request {
    fn id(&self) -> &Value {
        match self {
            Self::Status { id }
            | Self::Search { id, .. }
            | Self::Reload { id }
            | Self::Shutdown { id } => id,
        }
    }
}

pub fn serve(dir: &Path, mut input: impl BufRead, mut output: impl Write) -> Result<()> {
    let mut index: Option<Index> = None;
    loop {
        // fill_buf avoids unbounded allocation from a client that never sends a newline.
        let mut line = Vec::new();
        let mut oversized = false;
        let mut eof = false;
        loop {
            let buf = input.fill_buf()?;
            if buf.is_empty() {
                eof = true;
                break;
            }
            let end = buf.iter().position(|b| *b == b'\n').map(|i| i + 1);
            let n = end.unwrap_or(buf.len());
            if line.len() + n > MAX_REQUEST {
                oversized = true;
            } else if !oversized {
                line.extend_from_slice(&buf[..n]);
            }
            input.consume(n);
            if end.is_some() {
                break;
            }
        }
        if line.is_empty() && !oversized && eof {
            break;
        }
        let mut stop = false;
        let response = if oversized {
            json!({"id":null,"ok":false,"error":"request exceeds 65536 bytes"})
        } else {
            match serde_json::from_slice::<Request>(&line) {
                Err(error) => json!({"id":null,"ok":false,"error":error.to_string()}),
                Ok(request) => {
                    let id = request.id().clone();
                    let result = (|| -> Result<Value> {
                        match request {
                            Request::Status { .. } => Ok(
                                json!({"version":env!("CARGO_PKG_VERSION"),"protocol":1,"transport":"inherited_stdio","network":false,"loaded":index.is_some(),"entries":index.as_ref().map(Index::len),"generation":index.as_ref().map(Index::generation)}),
                            ),
                            Request::Reload { .. } => {
                                let fresh = operations::load_index(dir)?;
                                let len = fresh.len();
                                let generation = fresh.generation();
                                index = Some(fresh);
                                Ok(json!({"entries":len,"generation":generation}))
                            }
                            Request::Search { query, options, .. } => {
                                // Revalidate the live settings token for every request; shrinking scope revokes old readers.
                                let settings = operations::load_settings(dir)?;
                                let generation =
                                    crate::persistence::Store::open(dir.join("index"))?
                                        .current_generation()?;
                                if index.as_ref().map(Index::generation) != generation
                                    || index.is_none()
                                {
                                    index = Some(operations::load_index(dir)?)
                                }
                                let current = index.as_ref().unwrap();
                                operations::validate_index_scope(current, &settings)?;
                                let response = current.search(&query, &options, None)?;
                                operations::validate_index_scope(
                                    current,
                                    &operations::load_settings(dir)?,
                                )?;
                                Ok(serde_json::to_value(response)?)
                            }
                            Request::Shutdown { .. } => {
                                stop = true;
                                Ok(json!({"shutdown":true}))
                            }
                        }
                    })();
                    match result {
                        Ok(value) => json!({"id":id,"ok":true,"result":value}),
                        Err(error) => json!({"id":id,"ok":false,"error":format!("{error:#}")}),
                    }
                }
            }
        };
        serde_json::to_writer(&mut output, &response)?;
        output.write_all(b"\n")?;
        output.flush()?;
        if stop || eof {
            break;
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_request_is_bounded_and_next_line_survives() {
        let mut input = vec![b'x'; MAX_REQUEST + 1];
        input.extend_from_slice(b"\n{\"method\":\"status\",\"id\":2}\n");
        let mut out = Vec::new();
        serve(Path::new("/unused"), std::io::Cursor::new(input), &mut out).unwrap();
        let responses: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["ok"], false);
        assert_eq!(responses[1]["id"], 2);
        assert_eq!(responses[1]["ok"], true);
    }
    #[test]
    fn invalid_method_is_not_a_command() {
        let mut out = Vec::new();
        serve(
            Path::new("/unused"),
            std::io::Cursor::new(b"{\"method\":\"exec\",\"id\":1,\"command\":\"rm\"}\n"),
            &mut out,
        )
        .unwrap();
        assert!(String::from_utf8(out).unwrap().contains("unknown variant"));
    }
}
