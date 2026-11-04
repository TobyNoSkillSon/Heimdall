use anyhow::Result;
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    anyhow::ensure!(args.len() == 3, "expected input.wav output.wav bitrate");
    let mut wav = hound::WavReader::open(&args[0])?;
    anyhow::ensure!(
        wav.spec().channels == 1 && wav.spec().sample_rate == 16000,
        "expected mono 16k"
    );
    let audio = wav.samples::<i16>().collect::<Result<Vec<_>, _>>()?;
    let round = speakers::codec::roundtrip(&audio, args[2].parse()?)?;
    let mut out = hound::WavWriter::create(&args[1], wav.spec())?;
    for x in round.decoded {
        out.write_sample(x)?;
    }
    out.finalize()?;
    Ok(())
}
