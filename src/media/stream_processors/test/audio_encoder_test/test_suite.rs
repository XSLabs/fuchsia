// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![allow(clippy::large_futures)]

use crate::pcm_audio::*;
use crate::timestamp_validator::*;
use fidl_fuchsia_media::*;
use fidl_fuchsia_sysmem2::*;

use rand::prelude::*;
use std::rc::Rc;
use stream_processor_encoder_factory::*;
use stream_processor_test::*;

pub const TEST_PCM_FRAME_COUNT: usize = 3000;

pub struct AudioEncoderTestCase {
    // Encoder settings. This is a function because FIDL unions are not Copy or Clone.
    pub settings: EncoderSettings,
    /// The number of PCM input frames per encoded frame.
    pub input_framelength: usize,
    /// Sampling frequency to use for generating input for timestamp-related tests.
    pub input_frames_per_second: u32,
    pub channel_count: usize,
    pub output_tests: Vec<AudioEncoderOutputTest>,
}

/// An output test runs audio through the encoder and checks that the output is expected.
/// It checks the output size and if the hash was passed in, it checks that all that data
/// emitted when hashed sequentially results in the expected digest. Oob bytes are hashed first.
pub struct AudioEncoderOutputTest {
    /// If provided, the output will also be written to this file. Use this to verify new files
    /// with a decoder before using their digest in tests.
    pub output_file: Option<&'static str>,
    pub input_audio: PcmAudio,
    pub expected_output_size: OutputSize,
    pub expected_digests: Option<Vec<ExpectedDigest>>,
}

impl AudioEncoderOutputTest {
    pub fn saw_wave_test(
        frames_per_second: u32,
        expected_output_size: OutputSize,
        expected_digests: Vec<ExpectedDigest>,
    ) -> Self {
        Self {
            output_file: None,
            input_audio: PcmAudio::create_saw_wave(
                PcmFormat {
                    pcm_mode: AudioPcmMode::Linear,
                    bits_per_sample: 16,
                    frames_per_second,
                    channel_map: vec![AudioChannelId::Cf],
                },
                TEST_PCM_FRAME_COUNT,
            ),
            expected_output_size,
            expected_digests: Some(expected_digests),
        }
    }
}

impl AudioEncoderTestCase {
    pub async fn run(self) -> Result<()> {
        self.test_termination().await?;
        self.test_early_termination().await?;
        self.test_timestamps().await?;
        self.test_outputs().await
    }

    async fn test_outputs(self) -> Result<()> {
        let mut cases = vec![];
        let easy_framelength = self.input_framelength;
        for (output_test, stream_lifetime_ordinal) in
            self.output_tests.into_iter().zip(OrdinalPattern::Odd.into_iter())
        {
            let settings = self.settings.clone();
            let pcm_audio = output_test.input_audio;
            let stream = Rc::new(PcmAudioStream {
                pcm_audio,
                encoder_settings: settings.clone(),
                frames_per_packet: (0..).map(move |_| easy_framelength),
                timebase: None,
            });
            let mut validators: Vec<Rc<dyn OutputValidator>> =
                vec![Rc::new(TerminatesWithValidator {
                    expected_terminal_output: Output::Eos { stream_lifetime_ordinal },
                })];
            match output_test.expected_output_size {
                OutputSize::PacketCount(v) => {
                    validators.push(Rc::new(OutputPacketCountValidator {
                        expected_output_packet_count: v,
                    }));
                }
                OutputSize::RawBytesCount(v) => {
                    validators
                        .push(Rc::new(OutputDataSizeValidator { expected_output_data_size: v }));
                }
            }

            if let Some(expected_digests) = output_test.expected_digests {
                validators.push(Rc::new(BytesValidator {
                    output_file: output_test.output_file,
                    expected_digests,
                }));
            }
            cases.push(TestCase {
                name: "Audio encoder output test",
                stream,
                validators,
                stream_options: Some(StreamOptions {
                    queue_format_details: false,
                    ..StreamOptions::default()
                }),
            });
        }

        let spec = TestSpec {
            cases,
            relation: CaseRelation::Serial,
            stream_processor_factory: Rc::new(EncoderFactory),
        };

        spec.run().await.map(|_| ())
    }

    async fn test_termination(&self) -> Result<()> {
        let easy_framelength = self.input_framelength;
        let stream = self.create_test_stream((0..).map(move |_| easy_framelength));
        let eos_validator = Rc::new(TerminatesWithValidator {
            expected_terminal_output: Output::Eos { stream_lifetime_ordinal: 1 },
        });

        let case = TestCase {
            name: "Terminates with EOS test",
            stream,
            validators: vec![eos_validator],
            stream_options: None,
        };

        let spec = TestSpec {
            cases: vec![case],
            relation: CaseRelation::Concurrent,
            stream_processor_factory: Rc::new(EncoderFactory),
        };

        spec.run().await.map(|_| ())
    }

    async fn test_early_termination(&self) -> Result<()> {
        let easy_framelength = self.input_framelength;
        let stream = self.create_test_stream((0..).map(move |_| easy_framelength));
        let count_validator =
            Rc::new(OutputPacketCountValidator { expected_output_packet_count: 1 });

        // Pick an output packet size likely not divisible by any output codec frame size, to test
        // that half filled output packets are cleaned up in the codec without error when the
        // client disconnects early.
        const ODD_OUTPUT_PACKET_SIZE: u64 = 4096 - 1;

        let stream_options = Some(StreamOptions {
            output_buffer_collection_constraints: Some(BufferCollectionConstraints {
                buffer_memory_constraints: Some(BufferMemoryConstraints {
                    min_size_bytes: Some(ODD_OUTPUT_PACKET_SIZE),
                    ..buffer_memory_constraints_default()
                }),
                ..buffer_collection_constraints_default()
            }),
            stop_after_first_output: true,
            ..StreamOptions::default()
        });
        let case = TestCase {
            name: "Early termination test",
            stream,
            validators: vec![count_validator],
            stream_options,
        };

        let spec = TestSpec {
            cases: vec![case],
            relation: CaseRelation::Concurrent,
            stream_processor_factory: Rc::new(EncoderFactory),
        };

        spec.run().await.map(|_| ())
    }

    async fn test_timestamps(&self) -> Result<()> {
        let max_framelength = self.input_framelength * 5;

        let fixed_framelength = self.input_framelength + 1;
        let fixed_framelength_stream =
            self.create_test_stream((0..).map(move |_| fixed_framelength));
        let pcm_frame_size = fixed_framelength_stream.pcm_audio.frame_size();

        let stream_options = Some(StreamOptions {
            input_buffer_collection_constraints: Some(BufferCollectionConstraints {
                buffer_memory_constraints: Some(BufferMemoryConstraints {
                    min_size_bytes: Some((max_framelength * pcm_frame_size) as u64),
                    ..buffer_memory_constraints_default()
                }),
                ..buffer_collection_constraints_default()
            }),
            ..StreamOptions::default()
        });

        let fixed_framelength_case = TestCase {
            name: "Timestamp extrapolation test - fixed framelength",
            validators: vec![Rc::new(TimestampValidator::new(
                self.input_framelength,
                pcm_frame_size,
                fixed_framelength_stream.timestamp_generator(),
                fixed_framelength_stream.as_ref(),
            ))],
            stream: fixed_framelength_stream,
            stream_options: stream_options.clone(),
        };

        let variable_framelength_stream = self.create_test_stream((0..).map(move |i| {
            let mut rng = StdRng::seed_from_u64(i as u64);
            rng.gen::<usize>() % max_framelength + 1
        }));
        let variable_framelength_case = TestCase {
            name: "Timestamp extrapolation test - variable framelength",
            validators: vec![Rc::new(TimestampValidator::new(
                self.input_framelength,
                pcm_frame_size,
                variable_framelength_stream.timestamp_generator(),
                variable_framelength_stream.as_ref(),
            ))],
            stream: variable_framelength_stream,
            stream_options,
        };

        let spec = TestSpec {
            cases: vec![fixed_framelength_case, variable_framelength_case],
            relation: CaseRelation::Concurrent,
            stream_processor_factory: Rc::new(EncoderFactory),
        };

        spec.run().await.map(|_| ())
    }

    fn create_test_stream(
        &self,
        frames_per_packet: impl Iterator<Item = usize> + Clone,
    ) -> Rc<PcmAudioStream<impl Iterator<Item = usize> + Clone>> {
        let pcm_format = PcmFormat {
            pcm_mode: AudioPcmMode::Linear,
            bits_per_sample: 16,
            frames_per_second: self.input_frames_per_second,
            channel_map: match self.channel_count {
                1 => vec![AudioChannelId::Cf],
                2 => vec![AudioChannelId::Lf, AudioChannelId::Rf],
                c => panic!("{} is not a valid channel count", c),
            },
        };
        let pcm_audio = PcmAudio::create_saw_wave(pcm_format.clone(), TEST_PCM_FRAME_COUNT);
        let settings = self.settings.clone();
        Rc::new(PcmAudioStream {
            pcm_audio,
            encoder_settings: settings.clone(),
            frames_per_packet: frames_per_packet,
            timebase: Some(zx::MonotonicDuration::from_seconds(1).into_nanos() as u64),
        })
    }
}
