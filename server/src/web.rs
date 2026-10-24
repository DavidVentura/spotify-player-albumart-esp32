//! Browser UI: tuning sliders, what to show (song, presets, uploads, GIFs)
//! and a preview of the frame the panel is being sent.

use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::Arc;

use album_art_core::color::{Brightness, GammaExponent, Tuning, WhiteBalance};
use album_art_core::protocol::{Frame, SIZE};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use image::{ImageFormat, RgbImage};
use log::info;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::playback::{Animation, Repeat, Source};
use crate::presets::Presets;
use crate::scale;

const PAGE: &str = include_str!("web.html");
/// Long GIFs such as music videos run to tens of MiB.
const MAX_UPLOAD: usize = 256 * 1024 * 1024;

#[derive(Clone)]
struct AppState {
    tuning: Arc<watch::Sender<Tuning>>,
    source: Arc<watch::Sender<Source>>,
    output: watch::Receiver<Arc<Frame>>,
    presets: Arc<Presets>,
}

#[derive(Deserialize)]
struct AnimationQuery {
    repeat: Repeat,
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
    source: Arc<watch::Sender<Source>>,
    output: watch::Receiver<Arc<Frame>>,
    presets: Arc<Presets>,
) -> std::io::Result<()> {
    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/tuning", get(get_tuning).put(put_tuning))
        .route("/frame.png", get(frame_png))
        .route("/presets", get(list_presets))
        .route("/presets/{name}", get(preset_image))
        .route("/show/song", post(show_song))
        .route("/show/clear", post(show_clear))
        .route("/show/preset/{name}", post(show_preset))
        .route("/show/image", put(show_image))
        .route("/show/animation", put(show_animation))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD))
        .with_state(AppState {
            tuning,
            source,
            output,
            presets,
        });
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

async fn frame_png(State(state): State<AppState>) -> Response {
    let png = encode_png(&state.output.borrow());
    ([(header::CONTENT_TYPE, "image/png")], png).into_response()
}

async fn list_presets(State(state): State<AppState>) -> Json<Vec<String>> {
    Json(state.presets.keys().cloned().collect())
}

async fn preset_image(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    let Some(preset) = state.presets.get(&name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let format = image::guess_format(&preset.original).expect("presets were decoded at startup");
    (
        [(header::CONTENT_TYPE, format.to_mime_type())],
        preset.original.clone(),
    )
        .into_response()
}

async fn show_song(State(state): State<AppState>) -> StatusCode {
    state.source.send_replace(Source::Song);
    StatusCode::NO_CONTENT
}

async fn show_clear(State(state): State<AppState>) -> StatusCode {
    state
        .source
        .send_replace(Source::Still(Arc::new(Frame::black())));
    StatusCode::NO_CONTENT
}

async fn show_preset(State(state): State<AppState>, Path(name): Path<String>) -> StatusCode {
    let Some(preset) = state.presets.get(&name) else {
        return StatusCode::NOT_FOUND;
    };
    state
        .source
        .send_replace(Source::Still(Arc::clone(&preset.frame)));
    StatusCode::NO_CONTENT
}

/// Shows any still image until something else is selected.
async fn show_image(State(state): State<AppState>, image: Bytes) -> Response {
    let decoded = tokio::task::spawn_blocking(move || scale::to_frame(&image))
        .await
        .expect("decoding does not panic");
    match decoded {
        Ok(frame) => {
            state.source.send_replace(Source::Still(Arc::new(frame)));
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()).into_response(),
    }
}

async fn show_animation(
    State(state): State<AppState>,
    Query(query): Query<AnimationQuery>,
    gif: Bytes,
) -> Response {
    let decoded = tokio::task::spawn_blocking(move || scale::to_animation(&gif))
        .await
        .expect("decoding does not panic");
    let frames = match decoded {
        Ok(frames) => frames,
        Err(e) => return (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()).into_response(),
    };
    let Some(animation) = Animation::new(frames, query.repeat) else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "the GIF has no frames").into_response();
    };
    info!("Web: playing a {}-frame animation", animation.len());
    state.source.send_replace(Source::Animation(animation));
    StatusCode::NO_CONTENT.into_response()
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
