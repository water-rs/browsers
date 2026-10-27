The WPE WebKit pin in `browser-wpe/runtime/source.toml` is `$pinned`, which is
behind upstream.

## Stable releases newer than the pin

$releases

## Security advisories affecting the pin

$advisories

To move the pin, update `version`, `source_url`, `source_sha256` and
`released` in `browser-wpe/runtime/source.toml` and put the runtime build
through review. This issue is maintained by the weekly `wpe-release-watch`
workflow: it keeps naming the newest upstream release, and closing it without
bumping just files a fresh one next week.

Last checked $check_date against <https://wpewebkit.org/>.
