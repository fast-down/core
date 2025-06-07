use crate::args::DownloadArgs;
use crate::manager::Manager;
use crate::persist::DatabaseEntry;
use crate::{fmt, progress};
use color_eyre::Result;
use fast_down::Total;
use slint::{Model, SharedString, VecModel};
use std::{
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock,
    },
    thread::{self, JoinHandle},
};

slint::include_modules!();

impl From<&DatabaseEntry> for DownloadData {
    fn from(value: &DatabaseEntry) -> Self {
        let downloaded = value.progress.total();
        let speed = downloaded as f64 / value.elapsed as f64 * 1e3;
        let remaining_time = if speed > 0.0 {
            let remaining = (value.total_size - downloaded) as f64 / speed;
            fmt::format_time(remaining as u64)
        } else {
            "NaN".into()
        };
        Self {
            elapsed: fmt::format_time(value.elapsed / 1000).into(),
            file_name: value.file_name.clone().into(),
            is_downloading: false,
            percentage: format!(
                "{:.2}%",
                downloaded as f64 / value.total_size as f64 * 100.0
            )
            .into(),
            progress: Rc::new(VecModel::from(progress::add_blank(
                &value.progress,
                value.total_size,
            )))
            .into(),
            remaining_time: remaining_time.into(),
            speed: format!("{}/s", fmt::format_size(speed)).into(),
        }
    }
}

pub fn home_page(args: DownloadArgs) -> Result<()> {
    let args = Arc::new(args);
    dbg!(&args);
    let ui = AppWindow::new()?;
    let manager = Manager::new(args.clone());
    let download_list_model = Rc::new(VecModel::from(Vec::<DownloadData>::new()));
    ui.set_download_list(download_list_model.into());

    ui.on_add_url({
        let download_list_model = download_list_model.clone();
        let manager = manager.clone();
        let ui_handle = ui.as_weak();
        let download_handles = download_handles.clone();
        move |url| {
            dbg!(&url);
            let progress_model = Rc::new(VecModel::from(vec![]));
            let item_idx = download_list_model.row_count(); // Get the index before inserting
            download_list_model.insert(
                item_idx, // Insert at the end
                DownloadData {
                    elapsed: SharedString::from("00:00:00"),
                    file_name: SharedString::from(url.as_str()),
                    is_downloading: true,
                    percentage: SharedString::from("0.00%"),
                    progress: progress_model.into(),
                    remaining_time: SharedString::from("NaN"),
                    speed: SharedString::from("0.00 B/s"),
                },
            );

            let cancellation_token = Arc::new(AtomicBool::new(true));
            let manager_clone = manager.clone();
            let ui_handle_clone = ui_handle.clone();
            let download_list_model_clone = download_list_model.clone();
            let url_clone = url.clone();

            let handle = thread::spawn(move || {
                match manager_clone.lock().unwrap().download(
                    url.as_str(),
                    item_idx,
                    cancellation_token.clone(),
                    ui_handle_clone,
                    download_list_model_clone,
                ) {
                    Err(err) => {
                        dbg!(&err);
                        Err(err) // Return the error
                    }
                    Ok(_) => {
                        println!("下载成功");
                        Ok(()) // Return Ok(())
                    }
                }
            });
            download_handles
                .write()
                .unwrap()
                .insert(item_idx, (cancellation_token, handle));
        }
    });

    ui.on_stop({
        let manager = manager.clone();
        let download_list_model = download_list_model.clone();
        let download_handles = download_handles.clone();
        move |index| {
            let item_idx = index as usize;
            if let Some((cancellation_token, handle)) =
                download_handles.write().unwrap().get_mut(item_idx)
            {
                cancellation_token.store(false, Ordering::SeqCst);
                // Optionally, join the thread to ensure it finishes cleanly
                // if let Ok(res) = handle.join() {
                //     if let Err(e) = res {
                //         dbg!(&e);
                //     }
                // }
            }

            let mut data = download_list_model.row_data(item_idx).unwrap();
            data.is_downloading = false;
            download_list_model.set_row_data(item_idx, data);

            // Update manager's internal state
            if let Err(err) = manager.lock().unwrap().pause(item_idx) {
                dbg!(&err);
            }
        }
    });

    ui.on_resume({
        let manager = manager.clone();
        let download_list_model = download_list_model.clone();
        let ui_handle = ui.as_weak();
        let download_handles = download_handles.clone();
        move |index| {
            let item_idx = index as usize;
            let mut data = download_list_model.row_data(item_idx).unwrap();
            data.is_downloading = true;
            download_list_model.set_row_data(item_idx, data);

            match manager.lock().unwrap().resume(item_idx) {
                Ok(Some(progress_to_resume)) => {
                    let cancellation_token = Arc::new(AtomicBool::new(true));
                    let manager_clone = manager.clone();
                    let ui_handle_clone = ui_handle.clone();
                    let download_list_model_clone = download_list_model.clone();
                    let url = progress_to_resume.url.clone();

                    let handle = thread::spawn(move || {
                        match manager_clone.lock().unwrap().download(
                            url.as_str(),
                            item_idx,
                            cancellation_token.clone(),
                            ui_handle_clone,
                            download_list_model_clone,
                        ) {
                            Err(err) => {
                                dbg!(&err);
                                Err(err) // Return the error
                            }
                            Ok(_) => {
                                println!("下载成功");
                                Ok(()) // Return Ok(())
                            }
                        }
                    });
                    // Update the existing handle and token
                    download_handles
                        .write()
                        .unwrap()
                        .insert(item_idx, (cancellation_token, handle));
                }
                Ok(None) => {
                    println!("无法恢复下载：项目不在暂停状态");
                }
                Err(err) => {
                    dbg!(&err);
                }
            }
        }
    });

    ui.run()?;
    Ok(())
}
