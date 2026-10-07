# Directly use the npm distributed with Node, avoiding the broken global launcher.
$ErrorActionPreference = 'Stop'
$node = (Get-Command node.exe).Source
$npmCli = Join-Path (Split-Path $node -Parent) 'node_modules/npm/bin/npm-cli.js'
if (-not (Test-Path -LiteralPath $npmCli)) { throw 'Bundled npm-cli.js not found; do not install globally.' }
& $node $npmCli @args
exit $LASTEXITCODE
