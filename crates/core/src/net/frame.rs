//! Length-prefixed JSON framing shared by every TCP service.
//!
//! Wire format: `u32` little-endian payload length, then that many bytes of
//! UTF-8 JSON.
//!
//! Both legacy trees got this wrong in different ways. `Bifrost v3` read
//! straight into a fixed buffer with one `read()` call, so any message split
//! across TCP segments was truncated and any two messages coalesced into one
//! segment were lost. `Bifrost v2` framed correctly but trusted the length
//! word, so a hostile or corrupt peer could ask for a multi-gigabyte
//! allocation. Both are fixed here.

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::net::protocol::MAX_FRAME_BYTES;

/// Serialise `value` to JSON and write it as one length-prefixed frame.
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize + ?Sized,
{
    let bytes = serde_json::to_vec(value).context("failed to serialise frame")?;
    if bytes.len() as u64 > MAX_FRAME_BYTES as u64 {
        bail!(
            "frame of {} bytes exceeds the {MAX_FRAME_BYTES} byte limit",
            bytes.len()
        );
    }

    writer
        .write_u32_le(bytes.len() as u32)
        .await
        .context("failed to write frame length")?;
    writer
        .write_all(&bytes)
        .await
        .context("failed to write frame body")?;
    writer.flush().await.context("failed to flush frame")?;
    Ok(())
}

/// Read one length-prefixed frame and deserialise it.
///
/// Returns `Ok(None)` on a clean end-of-stream, so callers can loop until the
/// peer hangs up without treating a normal close as an error.
pub async fn read_frame<R, T>(reader: &mut R) -> Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let len = match reader.read_u32_le().await {
        Ok(len) => len,
        // A peer that closed cleanly between frames is not an error.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("failed to read frame length"),
    };

    if len > MAX_FRAME_BYTES {
        bail!("peer announced a {len} byte frame, over the {MAX_FRAME_BYTES} byte limit");
    }

    let mut buf = vec![0u8; len as usize];
    reader
        .read_exact(&mut buf)
        .await
        .context("failed to read frame body")?;

    let value = serde_json::from_slice(&buf)
        .with_context(|| format!("failed to parse a {len} byte JSON frame"))?;
    Ok(Some(value))
}

/// Read one frame, treating end-of-stream as an error.
///
/// Use where the protocol requires a reply, e.g. waiting for a [`FileAck`].
///
/// [`FileAck`]: crate::net::protocol::FileAck
pub async fn read_frame_required<R, T>(reader: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    read_frame(reader)
        .await?
        .context("peer closed the connection before replying")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Msg {
        id: u32,
        body: String,
    }

    #[tokio::test]
    async fn frames_round_trip_back_to_back() {
        let mut buf: Vec<u8> = Vec::new();
        for id in 0..3u32 {
            write_frame(
                &mut buf,
                &Msg {
                    id,
                    body: format!("message {id}"),
                },
            )
            .await
            .unwrap();
        }

        // Reading from one contiguous buffer models several messages arriving
        // in a single TCP segment — the case v3's bare read() dropped.
        let mut cursor = std::io::Cursor::new(buf);
        for id in 0..3u32 {
            let got: Msg = read_frame(&mut cursor)
                .await
                .unwrap()
                .expect("frame present");
            assert_eq!(
                got,
                Msg {
                    id,
                    body: format!("message {id}")
                }
            );
        }
        assert!(read_frame::<_, Msg>(&mut cursor).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn oversized_length_is_refused_without_allocating() {
        let mut raw = (MAX_FRAME_BYTES + 1).to_le_bytes().to_vec();
        raw.extend_from_slice(b"not actually this long");

        let mut cursor = std::io::Cursor::new(raw);
        let err = read_frame::<_, Msg>(&mut cursor).await.unwrap_err();
        assert!(
            err.to_string().contains("over the"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn truncated_body_is_an_error_not_a_silent_partial_read() {
        let mut raw = 64u32.to_le_bytes().to_vec();
        raw.extend_from_slice(b"only a few bytes");

        let mut cursor = std::io::Cursor::new(raw);
        assert!(read_frame::<_, Msg>(&mut cursor).await.is_err());
    }

    #[tokio::test]
    async fn clean_eof_yields_none() {
        let mut cursor = std::io::Cursor::new(Vec::new());
        assert!(read_frame::<_, Msg>(&mut cursor).await.unwrap().is_none());
    }
}
