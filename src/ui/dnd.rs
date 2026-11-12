//! Перетаскивание удалённых файлов наружу (в файловый менеджер).
//!
//! Приёмнику нужны локальные файлы, поэтому источник отдаёт `text/uri-list`
//! со ссылками на временные копии. Копии скачиваются лениво — когда
//! приёмник действительно запросил данные, — и только один раз за
//! перетаскивание.

use std::cell::RefCell;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use futures_util::FutureExt;
use futures_util::future::{LocalBoxFuture, Shared};
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib};

const URI_LIST: &str = "text/uri-list";

/// Скачивает выбранное во временный каталог и возвращает локальные пути.
pub type Fetch = Box<dyn Fn() -> LocalBoxFuture<'static, Result<Vec<PathBuf>, String>>>;

type UriListFuture = Shared<LocalBoxFuture<'static, Result<String, String>>>;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct RemoteFilesProvider {
        pub(super) fetch: RefCell<Option<Fetch>>,
        /// Общее для всех запросов скачивание (приёмник может запросить
        /// данные несколько раз).
        pub(super) pending: RefCell<Option<UriListFuture>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RemoteFilesProvider {
        const NAME: &'static str = "SshdeckRemoteFilesProvider";
        type Type = super::RemoteFilesProvider;
        type ParentType = gdk::ContentProvider;
    }

    impl ObjectImpl for RemoteFilesProvider {}

    impl ContentProviderImpl for RemoteFilesProvider {
        fn formats(&self) -> gdk::ContentFormats {
            gdk::ContentFormats::new(&[URI_LIST])
        }

        fn write_mime_type_future(
            &self,
            mime_type: &str,
            stream: &gio::OutputStream,
            io_priority: glib::Priority,
        ) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>> + 'static>> {
            if mime_type != URI_LIST {
                return Box::pin(async {
                    Err(glib::Error::new(
                        gio::IOErrorEnum::NotSupported,
                        tr!("Unsupported format"),
                    ))
                });
            }
            let uri_list = self.uri_list();
            let stream = stream.clone();
            Box::pin(async move {
                let text = uri_list
                    .await
                    .map_err(|message| glib::Error::new(gio::IOErrorEnum::Failed, &message))?;
                match stream
                    .write_all_future(text.into_bytes(), io_priority)
                    .await
                {
                    Ok((_, _, Some(err))) | Err((_, err)) => Err(err),
                    Ok((_, _, None)) => Ok(()),
                }
            })
        }
    }

    impl RemoteFilesProvider {
        fn uri_list(&self) -> UriListFuture {
            if let Some(pending) = self.pending.borrow().as_ref() {
                return pending.clone();
            }
            let download = self.fetch.borrow().as_ref().map(|fetch| fetch());
            let future: LocalBoxFuture<'static, Result<String, String>> = Box::pin(async move {
                let Some(download) = download else {
                    return Err(tr!("The data source is unavailable").to_string());
                };
                let paths = download.await?;
                let mut text = String::new();
                for path in paths {
                    let uri = glib::filename_to_uri(&path, None)
                        .map_err(|err| format!("{}: {err}", path.display()))?;
                    text.push_str(&uri);
                    text.push_str("\r\n");
                }
                Ok(text)
            });
            let shared = future.shared();
            *self.pending.borrow_mut() = Some(shared.clone());
            shared
        }
    }
}

glib::wrapper! {
    pub struct RemoteFilesProvider(ObjectSubclass<imp::RemoteFilesProvider>)
        @extends gdk::ContentProvider;
}

impl RemoteFilesProvider {
    pub fn new(fetch: Fetch) -> Self {
        let provider: Self = glib::Object::new();
        *provider.imp().fetch.borrow_mut() = Some(fetch);
        provider
    }
}
