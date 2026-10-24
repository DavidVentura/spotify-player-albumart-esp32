//! Browser UI: sliders for the panel tuning and a preview of the current frame.

use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::Arc;

use album_art_core::color::{Brightness, GammaExponent, Tuning, WhiteBalance};
use album_art_core::protocol::{Frame, SIZE};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use image::{ImageFormat, RgbImage};
use log::info;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::cover;

const PAGE: &str = include_str!("web.html");

#[derive(Clone)]
struct AppState {
    tuning: Arc<watch::Sender<Tuning>>,
    frames: Arc<watch::Sender<Arc<Frame>>>,
}

/// Wire form of [`Tuning`] for the page; validated on the way in.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct TuningJson {
    brightness: u8,
    gamma_hundredths: u16,
    red: u8,
    green: u8,
    blue: u8,
}

impl From<Tuning> for TuningJson {
    fn from(tuning: Tuning) -> Self {
        let (red, green, blue) = tuning.white_balance.percentages();
        TuningJson {
            brightness: tuning.brightness.percent(),
            gamma_hundredths: tuning.gamma.hundredths(),
            red,
            green,
            blue,
        }
    }
}

impl TryFrom<TuningJson> for Tuning {
    type Error = &'static str;

    fn try_from(json: TuningJson) -> Result<Self, Self::Error> {
        Ok(Tuning {
            brightness: Brightness::new(json.brightness).ok_or("brightness must be 0-100")?,
            gamma: GammaExponent::from_hundredths(json.gamma_hundredths)
                .ok_or("gamma must be 100-400 hundredths")?,
            white_balance: WhiteBalance::new(json.red, json.green, json.blue)
                .ok_or("white balance channels must be 0-100")?,
        })
    }
}

pub async fn serve(
    address: SocketAddr,
    tuning: Arc<watch::Sender<Tuning>>,
    frames: Arc<watch::Sender<Arc<Frame>>>,
) -> std::io::Result<()> {
    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/tuning", get(get_tuning).put(put_tuning))
        .route("/frame.png", get(frame_png))
        .route("/frame", axum::routing::put(put_frame))
        .with_state(AppState { tuning, frames });
    let listener = TcpListener::bind(address).await?;
    info!("Web: listening on http://{address}");
    axum::serve(listener, app).await
}

async fn get_tuning(State(state): State<AppState>) -> Json<TuningJson> {
    Json(TuningJson::from(*state.tuning.borrow()))
}

async fn put_tuning(State(state): State<AppState>, Json(json): Json<TuningJson>) -> Response {
    match Tuning::try_from(json) {
        Ok(tuning) => {
            state.tuning.send_replace(tuning);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(reason) => (StatusCode::UNPROCESSABLE_ENTITY, reason).into_response(),
    }
}

/// Shows any PNG or JPEG until the next track change replaces it.
async fn put_frame(State(state): State<AppState>, image: Bytes) -> Response {
    match cover::to_frame(&image) {
        Ok(frame) => {
            state.frames.send_replace(Arc::new(frame));
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()).into_response(),
    }
}

async fn frame_png(State(state): State<AppState>) -> Response {
    let png = encode_png(&state.frames.borrow());
    ([(header::CONTENT_TYPE, "image/png")], png).into_response()
}

fn encode_png(frame: &Frame) -> Vec<u8> {
    let image = RgbImage::from_fn(SIZE as u32, SIZE as u32, |x, y| {
        let pixel = frame.pixels[y as usize][x as usize];
        [pixel.r, pixel.g, pixel.b].into()
    });
    let mut png = Cursor::new(Vec::new());
    image
        .write_to(&mut png, ImageFormat::Png)
        .expect("encoding a 64x64 RGB image in memory cannot fail");
    png.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tuning_round_trips_through_json() {
        let json = TuningJson::from(Tuning::DEFAULT);
        assert_eq!(
            json,
            TuningJson {
                brightness: 20,
                gamma_hundredths: 220,
                red: 100,
                green: 100,
                blue: 100
            }
        );
        assert_eq!(Tuning::try_from(json), Ok(Tuning::DEFAULT));
    }

    #[test]
    fn rejects_out_of_range_tuning() {
        let json = TuningJson {
            brightness: 20,
            gamma_hundredths: 50,
            red: 100,
            green: 100,
            blue: 100,
        };
        assert!(Tuning::try_from(json).is_err());
    }

    #[test]
    fn preview_is_a_png_of_the_frame() {
        let png = encode_png(&Frame::black());
        let decoded = image::load_from_memory(&png).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (SIZE as u32, SIZE as u32));
        assert_eq!(decoded.get_pixel(5, 5).0, [0, 0, 0]);
    }
}
