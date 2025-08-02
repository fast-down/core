use crate::RandReader;
use bytes::Bytes;
use futures::{TryFutureExt, TryStream};
use reqwest::{Client, header};
use url::Url;

#[derive(Clone)]
pub struct ReqwestReader {
    pub(crate) client: Client,
    url: Url,
}

impl ReqwestReader {
    fn new(url: Url, client: Client) -> Self {
        Self { client, url }
    }
}

impl RandReader for ReqwestReader {
    type Error = reqwest::Error;
    fn read(
        &mut self,
        range: &crate::ProgressEntry,
    ) -> impl TryStream<Ok = Bytes, Error = Self::Error> + Send + Unpin {
        let req = self.client.get(self.url.clone()).header(
            header::RANGE,
            format!("bytes={}-{}", range.start, range.end - 1),
        );
        todo!();
        Box::pin(async move {
            let resp = req.send().await?;
            Ok(resp.bytes_stream())
        })
        .try_flatten_stream()
    }
}

// #[cfg(test)]
// mod tests {
//     use crate::base::reader::{Fetcher, Puller};
//     use crate::reqwest::reader::ClientFetcher;

//     #[tokio::test]
//     async fn test_reqwest_fetcher() {
//         let mut server = mockito::Server::new_async().await;
//         let _mock = server
//             .mock("GET", "/endpoint")
//             .with_status(200)
//             .with_body(b"Hello World")
//             .create_async()
//             .await;
//         let client = reqwest::Client::new();
//         let fetcher = ClientFetcher::builder(
//             client,
//             format!("{}/endpoint", server.url()).parse().unwrap(),
//         )
//         .build();
//         let mut puller = fetcher.fetch(0, None).await.unwrap();
//         let mut content = Vec::new();
//         while let Some(ref bytes) = puller.pull().await.unwrap() {
//             content.extend_from_slice(bytes);
//         }
//         assert_eq!(content, b"Hello World");
//     }
// }
