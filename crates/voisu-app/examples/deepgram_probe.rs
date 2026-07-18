//! Diagnostic probe: streams a raw s16le/16k/mono file to Deepgram at
//! real-time pace with the daemon's exact URL params and prints every
//! websocket event, including the server close frame reason.
//! Usage: DGKEY=... cargo run -p voisu-app --example deepgram_probe <file.raw>

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    voisu_app::system::install_crypto_provider();
    let path = std::env::args().nth(1).expect("usage: deepgram_probe <file.raw>");
    let key = std::env::var("DGKEY").expect("DGKEY env var");
    let pcm = std::fs::read(&path).expect("read audio file");
    let url = "wss://api.deepgram.com/v1/listen?model=nova-3&encoding=linear16&sample_rate=16000&channels=1&interim_results=true&smart_format=true&punctuate=true&endpointing=300&utterance_end_ms=1000";
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        format!("Token {key}").parse().unwrap(),
    );
    let started = std::time::Instant::now();
    let (socket, response) = tokio_tungstenite::connect_async(request).await.expect("connect");
    eprintln!("[{:>6.2}s] connected, HTTP {}", started.elapsed().as_secs_f32(), response.status());
    let (mut sink, mut stream) = socket.split();

    let sender = tokio::spawn(async move {
        for (index, chunk) in pcm.chunks(2048).enumerate() {
            if let Err(error) = sink.send(Message::Binary(chunk.to_vec())).await {
                eprintln!("[{:>6.2}s] SEND ERROR at chunk {index}: {error}", started.elapsed().as_secs_f32());
                return sink;
            }
            tokio::time::sleep(std::time::Duration::from_millis(64)).await;
        }
        eprintln!("[{:>6.2}s] all audio sent, sending CloseStream", started.elapsed().as_secs_f32());
        let _ = sink.send(Message::Text(r#"{"type":"CloseStream"}"#.to_owned())).await;
        sink
    });

    loop {
        match stream.next().await {
            Some(Ok(Message::Text(text))) => {
                let brief: String = text.chars().take(200).collect();
                eprintln!("[{:>6.2}s] TEXT {brief}", started.elapsed().as_secs_f32());
            }
            Some(Ok(Message::Close(frame))) => {
                eprintln!("[{:>6.2}s] CLOSE {frame:?}", started.elapsed().as_secs_f32());
                break;
            }
            Some(Ok(other)) => eprintln!("[{:>6.2}s] OTHER {other:?}", started.elapsed().as_secs_f32()),
            Some(Err(error)) => {
                eprintln!("[{:>6.2}s] WS ERROR {error}", started.elapsed().as_secs_f32());
                break;
            }
            None => {
                eprintln!("[{:>6.2}s] STREAM END", started.elapsed().as_secs_f32());
                break;
            }
        }
    }
    let _ = sender.await;
}
