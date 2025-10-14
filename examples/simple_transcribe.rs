/// Simple example that records for 3 seconds and transcribes
use sw1nn_transcription::transcription::Transcriber;
use std::thread;
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    env_logger::init();

    println!("Creating transcriber...");
    let transcriber = Transcriber::new("tiny".to_string(), "en".to_string())?;

    println!("Transcriber ready!");
    println!("\nSpeak now for 3 seconds...");

    // For this simple example, we'll just create some dummy audio data
    // In the real implementation, this would come from the microphone
    thread::sleep(Duration::from_secs(3));

    println!("\nProcessing...");

    // Create dummy silent audio (in real implementation this would be from mic)
    let sample_rate = 16000;
    let duration_secs = 3;
    let num_samples = sample_rate * duration_secs;
    let audio_data = vec![0.0f32; num_samples];

    let result = transcriber.transcribe(&audio_data)?;

    println!("Transcription result: {:?}", result);
    if result.is_empty() {
        println!("(No speech detected in silent audio - this is expected for this test)");
    }

    Ok(())
}
