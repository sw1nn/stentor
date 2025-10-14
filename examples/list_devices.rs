use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::{Context as PulseContext, FlagSet as ContextFlagSet};
use libpulse_binding::mainloop::threaded::Mainloop;
use std::cell::RefCell;
use std::rc::Rc;

fn main() {
    // Suppress ALSA warnings
    unsafe {
        std::env::set_var("LIBASOUND_THREAD_SAFE", "0");
    }

    println!("Available PulseAudio/PipeWire input sources:");
    println!("============================================\n");

    match list_sources() {
        Ok(sources) => {
            for (i, source) in sources.iter().enumerate() {
                println!("Source {}: {}", i + 1, source.description);
                println!("  Name: {}", source.name);
                println!("  Sample rate: {}", source.sample_rate);
                println!("  Channels: {}", source.channels);
                println!("  State: {}", source.state);
                println!();
            }

            if sources.is_empty() {
                println!("No input sources found");
            }
        }
        Err(e) => {
            eprintln!("Error listing sources: {}", e);
        }
    }

    // Show default source
    println!("\nDefault input source:");
    println!("====================");
    match get_default_source() {
        Ok(source) => {
            println!("{}", source.description);
            println!("  Name: {}", source.name);
        }
        Err(e) => {
            eprintln!("Error getting default source: {}", e);
        }
    }
}

struct SourceInfo {
    name: String,
    description: String,
    sample_rate: u32,
    channels: u8,
    state: String,
}

fn list_sources() -> Result<Vec<SourceInfo>, Box<dyn std::error::Error>> {
    let mut mainloop = Mainloop::new().ok_or("Failed to create mainloop")?;
    let mut context = PulseContext::new(&mainloop, "list-sources")
        .ok_or("Failed to create context")?;

    context.connect(None, ContextFlagSet::NOFLAGS, None)?;

    mainloop.lock();
    mainloop.start()?;

    // Wait for context to be ready
    loop {
        match context.get_state() {
            libpulse_binding::context::State::Ready => break,
            libpulse_binding::context::State::Failed
            | libpulse_binding::context::State::Terminated => {
                mainloop.unlock();
                mainloop.stop();
                return Err("PulseAudio context failed".into());
            }
            _ => {
                mainloop.unlock();
                std::thread::sleep(std::time::Duration::from_millis(10));
                mainloop.lock();
            }
        }
    }

    let sources = Rc::new(RefCell::new(Vec::new()));
    let sources_clone = Rc::clone(&sources);

    let introspect = context.introspect();
    introspect.get_source_info_list(move |result| match result {
        ListResult::Item(source_info) => {
            let name = source_info
                .name
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            let description = source_info
                .description
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "<no description>".to_string());
            let sample_rate = source_info.sample_spec.rate;
            let channels = source_info.sample_spec.channels;
            let state = format!("{:?}", source_info.state);

            sources_clone.borrow_mut().push(SourceInfo {
                name,
                description,
                sample_rate,
                channels,
                state,
            });
        }
        ListResult::End => {}
        ListResult::Error => {}
    });

    mainloop.unlock();
    std::thread::sleep(std::time::Duration::from_millis(100));

    mainloop.lock();
    let result = sources.borrow().clone();
    mainloop.unlock();

    mainloop.stop();

    Ok(result)
}

fn get_default_source() -> Result<SourceInfo, Box<dyn std::error::Error>> {
    let mut mainloop = Mainloop::new().ok_or("Failed to create mainloop")?;
    let mut context = PulseContext::new(&mainloop, "list-sources")
        .ok_or("Failed to create context")?;

    context.connect(None, ContextFlagSet::NOFLAGS, None)?;

    mainloop.lock();
    mainloop.start()?;

    // Wait for context to be ready
    loop {
        match context.get_state() {
            libpulse_binding::context::State::Ready => break,
            libpulse_binding::context::State::Failed
            | libpulse_binding::context::State::Terminated => {
                mainloop.unlock();
                mainloop.stop();
                return Err("PulseAudio context failed".into());
            }
            _ => {
                mainloop.unlock();
                std::thread::sleep(std::time::Duration::from_millis(10));
                mainloop.lock();
            }
        }
    }

    let default_source_name = Rc::new(RefCell::new(None));
    let default_source_name_clone = Rc::clone(&default_source_name);

    // Get server info to find default source
    let introspect = context.introspect();
    introspect.get_server_info(move |server_info| {
        if let Some(default_source) = server_info.default_source_name.as_ref() {
            *default_source_name_clone.borrow_mut() = Some(default_source.to_string());
        }
    });

    mainloop.unlock();
    std::thread::sleep(std::time::Duration::from_millis(100));

    mainloop.lock();
    let source_name = default_source_name
        .borrow()
        .clone()
        .ok_or("No default source")?;
    mainloop.unlock();

    // Get source info for default source
    let source_info = Rc::new(RefCell::new(None));
    let source_info_clone = Rc::clone(&source_info);

    mainloop.lock();
    let introspect = context.introspect();
    introspect.get_source_info_by_name(&source_name, move |list_result| {
        if let ListResult::Item(info) = list_result {
            let name = info
                .name
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            let description = info
                .description
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "<no description>".to_string());
            let sample_rate = info.sample_spec.rate;
            let channels = info.sample_spec.channels;
            let state = format!("{:?}", info.state);

            *source_info_clone.borrow_mut() = Some(SourceInfo {
                name,
                description,
                sample_rate,
                channels,
                state,
            });
        }
    });
    mainloop.unlock();

    std::thread::sleep(std::time::Duration::from_millis(100));

    mainloop.stop();

    source_info.borrow().clone().ok_or("Failed to get source info".into())
}

impl Clone for SourceInfo {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            description: self.description.clone(),
            sample_rate: self.sample_rate,
            channels: self.channels,
            state: self.state.clone(),
        }
    }
}
