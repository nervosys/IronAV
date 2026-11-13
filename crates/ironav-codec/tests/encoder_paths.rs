//! Every encoder is reachable as `<codec>::<Codec>Encoder` and as `encoders::<Codec>Encoder`
//! (the type is the same), and every decoder as `<codec>::<Codec>Decoder`.

use std::any::TypeId;

fn same<A: 'static, B: 'static>() {
    assert_eq!(TypeId::of::<A>(), TypeId::of::<B>());
}

#[test]
fn encoder_paths_are_consistent() {
    use ironav_codec::*;
    same::<aac::AacEncoder, encoders::AacEncoder>();
    same::<ac3::Ac3Encoder, encoders::Ac3Encoder>();
    same::<alac::AlacEncoder, encoders::AlacEncoder>();
    same::<av1::Av1Encoder, encoders::Av1Encoder>();
    same::<av1::enc::Av1Encoder, encoders::Av1Encoder>();
    same::<flac::FlacEncoder, encoders::FlacEncoder>();
    same::<h264::H264Encoder, encoders::H264Encoder>();
    same::<hevc::HevcEncoder, encoders::HevcEncoder>();
    same::<jpeg::JpegEncoder, encoders::JpegEncoder>();
    same::<mp3::Mp3Encoder, encoders::Mp3Encoder>();
    same::<mpeg2::Mpeg2Encoder, encoders::Mpeg2Encoder>();
    same::<mpeg2::enc::Mpeg2Encoder, encoders::Mpeg2Encoder>();
    same::<mpeg4::Mpeg4Encoder, encoders::Mpeg4Encoder>();
    same::<mpeg4::enc::Mpeg4Encoder, encoders::Mpeg4Encoder>();
    same::<opus::OpusEncoder, encoders::OpusEncoder>();
    same::<prores::ProResEncoder, encoders::ProResEncoder>();
    same::<vorbis::VorbisEncoder, encoders::VorbisEncoder>();
    same::<vp8::Vp8Encoder, encoders::Vp8Encoder>();
    same::<vp9::Vp9Encoder, encoders::Vp9Encoder>();
}

#[test]
fn decoder_paths_exist() {
    use ironav_codec::*;
    fn name<T>() -> &'static str {
        std::any::type_name::<T>()
    }
    let names = [
        name::<aac::AacDecoder>(), name::<ac3::Ac3Decoder>(), name::<alac::AlacDecoder>(), name::<av1::Av1Decoder>(),
        name::<flac::FlacDecoder>(), name::<h264::H264Decoder>(), name::<hevc::HevcDecoder>(), name::<jpeg::JpegDecoder>(),
        name::<mp3::Mp3Decoder>(), name::<mpeg2::Mpeg2Decoder>(), name::<mpeg4::Mpeg4Decoder>(), name::<opus::OpusDecoder>(),
        name::<prores::ProResDecoder>(), name::<vorbis::VorbisDecoder>(), name::<vp8::Vp8Decoder>(), name::<vp9::Vp9Decoder>(),
    ];
    assert_eq!(names.len(), 16);
}
