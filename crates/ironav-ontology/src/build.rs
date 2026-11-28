use crate::model::{entity, Kind, Ontology, Predicate, Status};
use ironav_core::{CodecId, MediaType, PixelFormat, SampleFormat};
use ironav_hwaccel::{linked_backends, providers_for, Direction, Platform, BACKENDS};
use serde_json::json;

fn codec_meta(c: CodecId) -> (&'static str, &'static str) {
    use CodecId::*;
    match c {
        PcmU8 => ("PCM unsigned 8-bit", "Uncompressed audio, 8-bit unsigned samples."),
        PcmS16Le => ("PCM signed 16-bit LE", "Uncompressed audio, 16-bit signed little-endian samples (CD quality)."),
        PcmS24Le => ("PCM signed 24-bit LE", "Uncompressed audio, 24-bit signed little-endian samples (studio / hi-res)."),
        PcmS32Le => ("PCM signed 32-bit LE", "Uncompressed audio, 32-bit signed little-endian samples."),
        PcmF32Le => ("PCM float 32-bit LE", "Uncompressed audio, 32-bit IEEE float samples."),
        PcmF64Le => ("PCM float 64-bit LE", "Uncompressed audio, 64-bit IEEE float samples."),
        Aac => ("AAC", "Lossy audio codec (MPEG-4 Part 3); default for MP4 and streaming. Decoded as AAC-LC, HE-AAC v1 (SBR) and HE-AAC v2 (SBR with parametric stereo); encoded as AAC-LC."),
        Mp3 => ("MP3", "Lossy audio codec (MPEG-1/2 Layer III); universal compatibility."),
        Mp2 => ("MP2", "Lossy audio codec (MPEG-1/2 Layer II), used in broadcast (DVB, DAB); decoded and encoded."),
        Ac3 => ("AC-3", "Lossy audio codec (ATSC A/52, Dolby Digital) used in DVD, broadcast and Blu-ray; decoded, not encoded."),
        Eac3 => ("E-AC-3", "Lossy audio codec (ATSC A/52 annex E, Dolby Digital Plus) of streaming services, broadcast and Blu-ray; decoded (the independent substream, up to 5.1 channels, with spectral extension and the adaptive hybrid transform), not encoded."),
        Dts => ("DTS", "Lossy audio codec (ETSI TS 102 114, DTS Coherent Acoustics, DCA) of DVD, Blu-ray and broadcast; the core substream is decoded (up to 5.1 channels; the DTS-HD extensions XCH, X96, XXCH, XBR, XLL and LBR are skipped, so DTS-HD streams give their core), not encoded."),
        TrueHd => ("TrueHD", "Lossless audio codec (Dolby TrueHD, built on MLP) of Blu-ray and streaming; decoded bit-exactly to up to 8 channels (the 2-, 6- and 8-channel presentations; the Dolby Atmos object substream is skipped, so Atmos streams give their 7.1 bed), not encoded."),
        Mlp => ("MLP", "Lossless audio codec (Meridian Lossless Packing) of DVD-Audio and the base of TrueHD; decoded bit-exactly (up to 6 channels, 16, 20 or 24 bits), not encoded."),
        WmaV1 => ("WMA 1", "Lossy audio codec (Windows Media Audio version 1, ASF format tag 0x160) of early .wma files; decoded (mono and stereo, the LSP and Vorbis-style exponent coding, noise substitution, block switching and the bit reservoir), not encoded."),
        WmaV2 => ("WMA 2", "Lossy audio codec (Windows Media Audio version 2, ASF format tag 0x161), the common .wma and ASF audio; decoded (mono and stereo, run-level coefficients, noise substitution, block switching, mid/side stereo and the bit reservoir), not encoded. WMA Pro, WMA Lossless and WMA Voice are different codecs and are not decoded."),
        Opus => ("Opus","Lossy low-latency audio codec (RFC 6716); best quality per bit for speech and music."),
        Vorbis => ("Vorbis", "Lossy audio codec used in Ogg and WebM."),
        Flac => ("FLAC", "Lossless audio compression."),
        Alac => ("ALAC", "Apple Lossless audio compression."),
        RawVideo => ("Raw video", "Uncompressed planar or packed video frames."),
        H264 => ("H.264 / AVC", "Lossy video codec (ITU-T H.264); the most widely supported and hardware-accelerated."),
        Hevc => ("H.265 / HEVC", "Lossy video codec; the successor to H.264, with smaller files at equal quality."),
        Av1 => ("AV1", "Royalty-free lossy video codec from AOMedia; best compression, growing hardware support."),
        Vp8 => ("VP8", "Lossy video codec used in early WebM."),
        Vp9 => ("VP9", "Royalty-free lossy video codec from Google; WebM and YouTube."),
        Mpeg4 => ("MPEG-4 Part 2", "Legacy lossy video codec (DivX/Xvid family)."),
        Mpeg2Video => ("MPEG-2 video", "Lossy video codec (ITU-T H.262 / ISO 13818-2) of broadcast, DVD and DVB; decoded (progressive and interlaced, 4:2:0 and 4:2:2) and encoded in software (progressive, 4:2:0)."),
        Mpeg1Video => ("MPEG-1 video", "Legacy lossy video codec (ISO 11172-2) of Video CD; decoded."),
        ProRes => ("Apple ProRes", "Intra-frame mezzanine codec for editing."),
        Mjpeg => ("Motion JPEG", "Video as a sequence of independent JPEG images."),
        SubRip => ("SubRip", "Timed text subtitles (.srt, and Matroska S_TEXT/UTF8); read as a subtitle stream of cues, not decoded to pictures."),
        Ass => ("ASS / SSA", "Advanced SubStation Alpha subtitles (.ass, .ssa, and Matroska S_TEXT/ASS): timed events with styling override tags; read as a subtitle stream of cues."),
        WebVtt => ("WebVTT", "Web Video Text Tracks subtitles (.vtt, and Matroska S_TEXT/WEBVTT); read as a subtitle stream of cues."),
        HdmvPgs => ("Blu-ray PGS subtitles", "Presentation Graphic Stream bitmap subtitles of Blu-ray discs (.sup, and Matroska S_HDMV/PGS); read as a subtitle stream of display sets and decoded to bitmaps for display; no text is recovered (no OCR)."),
        DvdSub => ("DVD VobSub subtitles", "DVD subpicture bitmap subtitles (VobSub; Matroska S_VOBSUB and DVD program streams); read as a subtitle stream of subpicture packets and decoded to bitmaps for display; no text is recovered (no OCR)."),
    }
}

/// Containers IronAV does not implement yet, with the codecs they hold.
const PLANNED_CONTAINERS: &[(&str, &str, &str, &[CodecId])] = &[
];

/// Build the ontology from the live registries.
pub fn build() -> Ontology {
    let mut o = Ontology::new();
    let platform_now = Platform::current();

    for p in Platform::ALL {
        o.add_entity(
            entity(Kind::Platform, p.name(), p.name(), "An operating system IronAV targets.", Status::Implemented)
                .with_prop("current", p == platform_now),
        );
    }

    for f in SampleFormat::ALL {
        o.add_entity(
            entity(Kind::SampleFormat, f.name(), f.name(), "Audio sample representation.", Status::Implemented)
                .with_prop("bytes", f.bytes() as u64),
        );
    }
    for f in PixelFormat::ALL {
        o.add_entity(
            entity(Kind::PixelFormat, f.name(), f.name(), "Video pixel layout.", Status::Implemented)
                .with_prop("planes", f.planes(16, 16).len() as u64),
        );
    }

    let implemented = ironav_codec::implemented();
    for &c in CodecId::ALL {
        let (label, desc) = codec_meta(c);
        let imp = implemented.iter().find(|i| i.codec == c);
        let media = match c.media_type() {
            MediaType::Audio => "audio",
            MediaType::Video => "video",
            MediaType::Subtitle => "subtitle",
        };
        // A codec with no software implementation is still usable if a hardware provider encodes it.
        let hw_encoders: Vec<&str> = providers_for(c, Direction::Encode).iter().map(|b| b.name()).collect();
        let hw_decoders: Vec<&str> = providers_for(c, Direction::Decode).iter().map(|b| b.name()).collect();
        // a subtitle codec is "implemented" when the container readers give its cues as packets: there is nothing to decode
        let usable = imp.is_some() || !hw_encoders.is_empty() || !hw_decoders.is_empty() || c.media_type() == MediaType::Subtitle;
        o.add_entity(
            entity(Kind::Codec, c.name(), label, desc, if usable { Status::Implemented } else { Status::Planned })
                .with_prop("media", media)
                .with_prop("software_decode", imp.is_some_and(|i| i.decode))
                .with_prop("software_encode", imp.is_some_and(|i| i.encode))
                .with_prop("hardware_encode_providers", json!(hw_encoders))
                .with_prop("hardware_decode_providers", json!(hw_decoders))
                .with_prop("cli_name", c.name()),
        );
        if let Some(sf) = c.sample_format() {
            o.add_relation(&format!("codec/{}", c.name()), Predicate::UsesSampleFormat, &format!("samplefmt/{}", sf.name()), None);
        }
    }

    for f in ironav_formats::FORMATS {
        o.add_entity(
            entity(Kind::Container, f.name, f.name, f.description, Status::Implemented)
                .with_prop("demux", f.demux)
                .with_prop("mux", f.mux)
                .with_prop("extensions", json!(f.extensions)),
        );
        for c in f.codecs {
            o.add_relation(&format!("container/{}", f.name), Predicate::Carries, &format!("codec/{}", c.name()), None);
        }
    }
    // one built since it was listed here keeps its implemented entry
    for (name, label, desc, codecs) in PLANNED_CONTAINERS.iter().filter(|p| !ironav_formats::FORMATS.iter().any(|f| f.name == p.0)) {
        o.add_entity(
            entity(Kind::Container, name, label, desc, Status::Planned).with_prop("demux", false).with_prop("mux", false),
        );
        for c in *codecs {
            o.add_relation(&format!("container/{name}"), Predicate::Carries, &format!("codec/{}", c.name()), None);
        }
    }

    for f in ironav_codec::FILTERS {
        o.add_entity(
            entity(Kind::Filter, f.name, f.name, f.description, Status::Implemented)
                .with_prop("media", if f.media == MediaType::Audio { "audio" } else { "video" })
                .with_prop("params", serde_json::to_value(f.params).unwrap_or_default()),
        );
        for p in f.input_pix_fmts {
            o.add_relation(&format!("filter/{}", f.name), Predicate::AcceptsPixelFormat, &format!("pixfmt/{}", p.name()), None);
        }
        if let Some(p) = f.output_pix_fmt {
            o.add_relation(&format!("filter/{}", f.name), Predicate::ProducesPixelFormat, &format!("pixfmt/{}", p.name()), None);
        }
    }

    let linked = linked_backends();
    for b in BACKENDS {
        let status = if linked.contains(&b.backend) { Status::Implemented } else { Status::Planned };
        o.add_entity(
            entity(Kind::HwBackend, b.name, b.label, &format!("{} hardware acceleration API.", b.vendor), status)
                .with_prop("vendor", b.vendor)
                .with_prop("compute_filters", b.compute)
                .with_prop("linked", linked.contains(&b.backend)),
        );
        for p in b.platforms {
            o.add_relation(&format!("hw/{}", b.name), Predicate::AvailableOn, &format!("platform/{}", p.name()), None);
        }
        for c in b.decode.iter().chain(b.encode) {
            let (dec, enc) = (b.decode.contains(c), b.encode.contains(c));
            // One relation per (backend, codec) even when both directions are supported.
            let note = match (dec, enc) {
                (true, true) => "decode+encode",
                (true, false) => "decode",
                _ => "encode",
            };
            let rel = (format!("hw/{}", b.name), format!("codec/{}", c.name()));
            if o.relations().iter().any(|r| r.predicate == Predicate::Accelerates && r.subject == rel.0 && r.object == rel.1) {
                continue;
            }
            o.add_relation(&rel.0, Predicate::Accelerates, &rel.1, Some(note));
        }
    }
    o
}
