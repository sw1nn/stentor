use sw1nn_transcription::transcription::Transcriber;

fn main() -> anyhow::Result<()> {
    env_logger::init();

    println!("Creating transcriber with tiny model...");
    let transcriber = Transcriber::new("tiny".to_string(), "en".to_string())?;

    println!("Transcriber created successfully!");
    println!("Language: {:?}", transcriber.get_language());

    Ok(())
}
