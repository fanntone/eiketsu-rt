//! Audio output (cpal: WASAPI / CoreAudio / ALSA). The OPL2 synthesizer runs on the
//! audio thread; the emulator hands it register writes through `sink`.

use crate::opl::Sink;

#[cfg(feature = "native")]
pub struct Audio {
    pub sink: Sink,
    _stream: cpal::Stream,
}

/// Built without native audio (the web build feeds its own synthesizer).
#[cfg(not(feature = "native"))]
pub struct Audio {
    pub sink: Sink,
}

#[cfg(not(feature = "native"))]
impl Audio {
    pub fn open() -> Option<Audio> {
        None
    }
}

#[cfg(feature = "native")]
impl Audio {
    /// None when there is no output device (the game still runs, silently).
    pub fn open() -> Option<Audio> {
        use crate::opl::Synth;
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        use std::sync::{Arc, Mutex};
        let dev = cpal::default_host().default_output_device()?;
        let cfg = dev.default_output_config().ok()?;
        let rate = cfg.sample_rate().0;
        let ch = cfg.channels() as usize;
        let sink: Sink = Arc::new(Mutex::new(Vec::new()));
        let q = sink.clone();
        let mut synth = Synth::new(rate);
        let mut tmp: Vec<f32> = Vec::new();
        let stream = dev
            .build_output_stream(
                &cfg.config(),
                move |out: &mut [f32], _| {
                    for (r, v) in q.lock().unwrap().drain(..) {
                        synth.write(r, v);
                    }
                    let frames = out.len() / ch;
                    tmp.resize(frames * 2, 0.0);
                    synth.generate(&mut tmp);
                    for (i, f) in out.chunks_mut(ch).enumerate() {
                        for (c, s) in f.iter_mut().enumerate() {
                            *s = tmp[i * 2 + (c & 1)];
                        }
                    }
                },
                |e| eprintln!("[audio] {e}"),
                None,
            )
            .ok()?;
        stream.play().ok()?;
        Some(Audio { sink, _stream: stream })
    }
}
