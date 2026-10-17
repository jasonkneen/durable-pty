use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Serialize)]
pub struct Response {
    pub id: Value,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn from(id: Value, r: Result<Value, String>) -> Self {
        match r {
            Ok(v) => Response { id, ok: true, result: Some(v), error: None },
            Err(e) => Response { id, ok: false, result: None, error: Some(e) },
        }
    }
}

// After an `attach` request is answered, the connection carries binary frames: [type u8][len u32 BE][payload].
// server -> client
pub const S_DATA: u8 = 0;
pub const S_EXIT: u8 = 1; // payload: i32 BE exit code
// client -> server
pub const C_INPUT: u8 = 0;
pub const C_RESIZE: u8 = 1; // payload: cols u16 BE, rows u16 BE
pub const C_DETACH: u8 = 2;

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, t: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(t);
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf).await?;
    w.flush().await
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<(u8, Vec<u8>)> {
    let mut h = [0u8; 5];
    r.read_exact(&mut h).await?;
    let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
    if len > 4 * 1024 * 1024 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut p = vec![0u8; len];
    r.read_exact(&mut p).await?;
    Ok((h[0], p))
}
