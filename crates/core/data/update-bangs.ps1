# Refresh the bundled bang list from the latest release of Kagi's bang repository
# (https://github.com/kagisearch/bangs, MIT), the same source Helium uses. Run it
# before a release and commit the three files it writes; the list is compiled into
# the browser, so it only changes when this is re-run.
$ErrorActionPreference = 'Stop'
$dir = $PSScriptRoot
$tag = (Invoke-RestMethod 'https://api.github.com/repos/kagisearch/bangs/releases')[0].tag_name
$base = "https://raw.githubusercontent.com/kagisearch/bangs/refs/tags/$tag"
Invoke-WebRequest "$base/data/bangs.json" -OutFile (Join-Path $dir 'kagi-bangs.json')
Invoke-WebRequest "$base/LICENSE" -OutFile (Join-Path $dir 'kagi-bangs.LICENSE')
[IO.File]::WriteAllText((Join-Path $dir 'kagi-bangs.version'), "$tag`n")
"bundled Kagi bangs $tag"
