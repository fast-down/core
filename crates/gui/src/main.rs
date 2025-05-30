// Prevent console window in addition to Slint window in Windows release builds when, e.g., starting the app via file manager. Ignored on other platforms.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::error::Error;

slint::include_modules!();

fn main() -> Result<(), Box<dyn Error>> {
    let ui = AppWindow::new()?;

    let mut download_list: Vec<DownloadData> = ui.get_download_list().iter().collect();
    let download_list_model = std::rc::Rc::new(slint::VecModel::from(download_list));
    ui.set_download_list(download_list_model.clone().into());

    // ui.on_add_url(move |url| {
    //     let ui_handle = ui.as_weak();
    //     move || {
    //         let ui = ui_handle.unwrap();
    //         ui.set_counter(ui.get_counter() + 1);
    //     }
    // });

    ui.run()?;

    Ok(())
}
