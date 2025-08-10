mod options;
mod pf;

use fast_pull::UrlInfo;
use url::Url;

trait Down {
    fn prefetch(&self, url: Url) -> ();
}
