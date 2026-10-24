//! Decides what the panel shows: the song's cover, a still, or an animation.

use std::sync::Arc;
use std::time::Duration;

use album_art_core::protocol::Frame;
use serde::Deserialize;
use tokio::sync::watch;
use tokio::time::{Instant, sleep_until};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    Once,
    Forever,
}

/// A non-empty sequence of frames, each with how long it stays on screen.
#[derive(Clone, Debug)]
pub struct Animation {
    frames: Arc<[(Arc<Frame>, Duration)]>,
    repeat: Repeat,
}

impl Animation {
    pub fn new(frames: Vec<(Frame, Duration)>, repeat: Repeat) -> Option<Self> {
        if frames.is_empty() {
            return None;
        }
        let frames = frames
            .into_iter()
            .map(|(frame, delay)| (Arc::new(frame), delay))
            .collect();
        Some(Animation { frames, repeat })
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    fn is_same(&self, other: &Animation) -> bool {
        Arc::ptr_eq(&self.frames, &other.frames)
    }
}

#[derive(Clone, Debug)]
pub enum Source {
    Song,
    Still(Arc<Frame>),
    Animation(Animation),
}

/// Forwards whichever source is selected to `output`. A finished one-shot
/// animation hands back to the song.
pub async fn run(
    source: Arc<watch::Sender<Source>>,
    mut covers: watch::Receiver<Arc<Frame>>,
    output: Arc<watch::Sender<Arc<Frame>>>,
) {
    let mut selected = source.subscribe();
    loop {
        let current = selected.borrow_and_update().clone();
        match current {
            Source::Song => {
                output.send_replace(Arc::clone(&covers.borrow_and_update()));
                tokio::select! {
                    changed = selected.changed() => changed.expect("the source sender lives in main"),
                    changed = covers.changed() => changed.expect("the cover sender lives in main"),
                }
            }
            Source::Still(frame) => {
                output.send_replace(frame);
                selected
                    .changed()
                    .await
                    .expect("the source sender lives in main");
            }
            Source::Animation(animation) => {
                tokio::select! {
                    () = play(&animation, &output) => {
                        source.send_if_modified(|current| match current {
                            Source::Animation(playing) if playing.is_same(&animation) => {
                                *current = Source::Song;
                                true
                            }
                            _ => false,
                        });
                    }
                    changed = selected.changed() => changed.expect("the source sender lives in main"),
                }
            }
        }
    }
}

/// Paces frames against a fixed timeline, so slow sends do not accumulate
/// into drift. Returns only for [`Repeat::Once`].
async fn play(animation: &Animation, output: &watch::Sender<Arc<Frame>>) {
    let mut due = Instant::now();
    loop {
        for (frame, delay) in animation.frames.iter() {
            output.send_replace(Arc::clone(frame));
            due += *delay;
            sleep_until(due).await;
        }
        if animation.repeat == Repeat::Once {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use album_art_core::color::Rgb;

    fn solid(level: u8) -> Frame {
        let mut frame = Frame::black();
        for row in frame.pixels.iter_mut() {
            row.fill(Rgb {
                r: level,
                g: level,
                b: level,
            });
        }
        frame
    }

    fn level(output: &watch::Receiver<Arc<Frame>>) -> u8 {
        output.borrow().pixels[0][0].r
    }

    struct Rig {
        source: Arc<watch::Sender<Source>>,
        covers: watch::Sender<Arc<Frame>>,
        output: watch::Receiver<Arc<Frame>>,
    }

    fn start() -> Rig {
        let source = Arc::new(watch::Sender::new(Source::Song));
        let covers = watch::Sender::new(Arc::new(solid(1)));
        let output = Arc::new(watch::Sender::new(Arc::new(Frame::black())));
        let output_rx = output.subscribe();
        tokio::spawn(run(Arc::clone(&source), covers.subscribe(), output));
        Rig {
            source,
            covers,
            output: output_rx,
        }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn song_follows_cover_changes() {
        let rig = start();
        settle().await;
        assert_eq!(level(&rig.output), 1);
        rig.covers.send_replace(Arc::new(solid(2)));
        settle().await;
        assert_eq!(level(&rig.output), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn still_ignores_covers_until_back_to_song() {
        let rig = start();
        rig.source.send_replace(Source::Still(Arc::new(solid(9))));
        settle().await;
        rig.covers.send_replace(Arc::new(solid(2)));
        settle().await;
        assert_eq!(level(&rig.output), 9);
        rig.source.send_replace(Source::Song);
        settle().await;
        assert_eq!(level(&rig.output), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn one_shot_animation_plays_its_timeline_then_returns_to_song() {
        let rig = start();
        let frames = vec![
            (solid(10), Duration::from_millis(100)),
            (solid(20), Duration::from_millis(300)),
        ];
        let animation = Animation::new(frames, Repeat::Once).unwrap();
        rig.source.send_replace(Source::Animation(animation));
        settle().await;
        assert_eq!(level(&rig.output), 10);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(level(&rig.output), 20);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(level(&rig.output), 1);
        assert!(matches!(*rig.source.borrow(), Source::Song));
    }

    #[tokio::test(start_paused = true)]
    async fn repeating_animation_loops_until_replaced() {
        let rig = start();
        let frames = vec![
            (solid(10), Duration::from_millis(100)),
            (solid(20), Duration::from_millis(100)),
        ];
        let animation = Animation::new(frames, Repeat::Forever).unwrap();
        rig.source.send_replace(Source::Animation(animation));
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(level(&rig.output), 10);
        rig.source.send_replace(Source::Still(Arc::new(solid(5))));
        settle().await;
        assert_eq!(level(&rig.output), 5);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(level(&rig.output), 5);
    }

    #[test]
    fn empty_animation_is_rejected() {
        assert!(Animation::new(Vec::new(), Repeat::Once).is_none());
    }
}
