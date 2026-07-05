use clap::Parser;
use log::LevelFilter;
use rebels::app::App;
use rebels::args::AppArgs;
#[cfg(feature = "relayer")]
use rebels::args::AppMode;
use rebels::logging;
#[cfg(feature = "relayer")]
use rebels::relayer::Relayer;
use rebels::tui::Tui;
use rebels::types::AppResult;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> AppResult<()> {
    logging::init(LevelFilter::Info)?;

    let args = AppArgs::parse();

    #[cfg(feature = "relayer")]
    let mode = args.app_mode();

    #[cfg(feature = "relayer")]
    if mode == AppMode::Relayer {
        return Relayer::new().run().await;
    }

    let ui_disabled = args.is_ui_disabled();
    let mut app = App::new(args)?;

    if ui_disabled {
        let tui = Tui::new_dummy()?;
        app.run(tui).await?;
    } else {
        let tui = Tui::new_local()?;
        app.run(tui).await?;
    };

    Ok(())
}
