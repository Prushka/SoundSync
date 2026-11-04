[CmdletBinding()]
param([string]$SourceDirectory, [string]$VcToolsDirectory)
$ErrorActionPreference = 'Stop'
$atmosExperiment = $PSScriptRoot
$atmosWorkspace = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if (!$SourceDirectory) { $SourceDirectory = Join-Path $atmosWorkspace 'target/atmos-driver-source' }
$atmosRoot = [IO.Path]::GetFullPath($SourceDirectory)
$atmosCommit = 'f4c1ff842b6a0001152f44cb61788b216a1c7627'
$atmosVersion = '10.0.28000.2526'
$atmosKitVersion = '10.0.28000.0'
function Invoke-AtmosNative([string]$Program, [string[]]$Arguments) {
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Program exited with $LASTEXITCODE" }
}

if (!(Test-Path -LiteralPath $atmosRoot)) {
    New-Item -ItemType Directory -Path (Split-Path -Parent $atmosRoot) -Force | Out-Null
    Invoke-AtmosNative git @('clone','--filter=blob:none','--sparse','https://github.com/microsoft/Windows-driver-samples.git',$atmosRoot)
    Invoke-AtmosNative git @('-C',$atmosRoot,'sparse-checkout','set','audio/sysvad')
    Invoke-AtmosNative git @('-C',$atmosRoot,'checkout','--detach',$atmosCommit)
}
$atmosActualCommit = & git -C $atmosRoot rev-parse HEAD
if ($LASTEXITCODE -ne 0 -or $atmosActualCommit -ne $atmosCommit) { throw "Source must be pinned to $atmosCommit" }
$atmosPatch = Join-Path $atmosExperiment 'sysvad.patch'
& git -C $atmosRoot apply --reverse --check $atmosPatch 2>$null
if ($LASTEXITCODE -ne 0) {
    Invoke-AtmosNative git @('-C',$atmosRoot,'apply','--check',$atmosPatch)
    Invoke-AtmosNative git @('-C',$atmosRoot,'apply',$atmosPatch)
}
$atmosExpectedDiff = (Get-Content -LiteralPath $atmosPatch -Raw).Replace("`r`n", "`n").TrimEnd()
$atmosActualDiff = ((& git -C $atmosRoot diff --no-ext-diff --binary) -join "`n").TrimEnd()
if ($LASTEXITCODE -ne 0 -or $atmosActualDiff -ne $atmosExpectedDiff) { throw 'The source checkout contains changes outside the reviewed patch' }

$atmosPackages = Join-Path $atmosRoot 'packages'
New-Item -ItemType Directory -Path $atmosPackages -Force | Out-Null
$atmosHashes = [ordered]@{
    'Microsoft.Windows.WDK.x64' = '63C939FB5A79295BF40E941DB592681272219B04EDFF095FE2F3D123E5579A90'
    'Microsoft.Windows.SDK.CPP.x64' = 'A9CAE2A8C5DA7F5DC5838AE6A76D06D0D2E2FDC3D8CFC69CA6C184E4B9193A00'
    'Microsoft.Windows.SDK.CPP' = 'BE1B419491607EAE6F7C57844EBAB39FACE9643C51E2AF1D9176A3BA0D0B23FC'
}
foreach ($atmosName in $atmosHashes.Keys) {
    $atmosArchive = Join-Path $atmosPackages "$atmosName.$atmosVersion.nupkg"
    if (!(Test-Path -LiteralPath $atmosArchive)) {
        $atmosLower = $atmosName.ToLowerInvariant()
        Invoke-WebRequest -Uri "https://api.nuget.org/v3-flatcontainer/$atmosLower/$atmosVersion/$atmosLower.$atmosVersion.nupkg" -OutFile $atmosArchive
    }
    if ((Get-FileHash -LiteralPath $atmosArchive -Algorithm SHA256).Hash -ne $atmosHashes[$atmosName]) { throw "Hash mismatch: $atmosArchive" }
    $atmosPackage = Join-Path $atmosPackages "$atmosName.$atmosVersion"
    if (!(Test-Path -LiteralPath (Join-Path $atmosPackage '.signature.p7s'))) {
        Expand-Archive -LiteralPath $atmosArchive -DestinationPath $atmosPackage -Force
    }
}

if (!$VcToolsDirectory) {
    $atmosVsWhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
    $atmosVs = & $atmosVsWhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if (!$atmosVs) { throw 'An existing MSVC x64 build toolchain is required; this script does not install one' }
    $atmosVcVersion = (Get-Content -LiteralPath (Join-Path $atmosVs 'VC/Auxiliary/Build/Microsoft.VCToolsVersion.default.txt') -Raw).Trim()
    $VcToolsDirectory = Join-Path $atmosVs "VC/Tools/MSVC/$atmosVcVersion"
}
$atmosVc = (Resolve-Path -LiteralPath $VcToolsDirectory).Path
$atmosCompiler = Join-Path $atmosVc 'bin/Hostx64/x64/cl.exe'
$atmosLinker = Join-Path $atmosVc 'bin/Hostx64/x64/link.exe'
$atmosWdk = Join-Path $atmosPackages "Microsoft.Windows.WDK.x64.$atmosVersion/c"
$atmosSdk = Join-Path $atmosPackages "Microsoft.Windows.SDK.CPP.$atmosVersion/c"
$atmosSdkLibraries = Join-Path $atmosPackages "Microsoft.Windows.SDK.CPP.x64.$atmosVersion/c"
$atmosSource = Join-Path $atmosRoot 'audio/sysvad'
$atmosOutput = Join-Path $atmosRoot 'local-build'
New-Item -ItemType Directory -Path $atmosOutput -Force | Out-Null
foreach ($atmosFile in @('capture-types.h','capture-buffer.h','capture-protocol.h','ss-atmos-capture.h','ss-atmos-capture.cpp')) {
    Copy-Item -LiteralPath (Join-Path $atmosExperiment $atmosFile) -Destination $atmosSource
}

# Call the installed compiler directly with the NuGet WDK; no VS integration install,
# certificate generation, signing, device installation, registry writes or boot changes.
$atmosIncludes = @(
    "$atmosWdk/Include/$atmosKitVersion/km/crt", "$atmosWdk/Include/$atmosKitVersion/km",
    "$atmosSdk/Include/$atmosKitVersion/shared", "$atmosWdk/Include/$atmosKitVersion/shared",
    "$atmosWdk/Include/wdf/kmdf/1.15", "$atmosVc/include",
    $atmosSource, "$atmosSource/EndpointsCommon", "$atmosSource/TabletAudioSample"
)
$atmosFlags = @('/nologo','/c','/kernel','/std:c++17','/O2','/Zl','/GR-','/GS','/guard:cf','/W3','/WX','/wd4595',
    '/D_WIN64','/D_AMD64_','/DAMD64','/D_WIN32_WINNT=0x0A00','/DNTDDI_VERSION=0x0A00000C','/DNT_PROCESSOR_GROUPS',
    '/D_USE_WAVERT_','/D_USE_IPortClsRuntimePower','/D_NEW_DELETE_OPERATORS_','/DSYSVAD_BTH_BYPASS','/DSYSVAD_USB_SIDEBAND')
$atmosFlags += $atmosIncludes | ForEach-Object { '/I' + $_ }
$atmosSources = @()
foreach ($atmosProject in @('EndpointsCommon/EndpointsCommon.vcxproj','TabletAudioSample/TabletAudioSample.vcxproj')) {
    $atmosProjectPath = Join-Path $atmosSource $atmosProject
    [xml]$atmosXml = Get-Content -LiteralPath $atmosProjectPath -Raw
    foreach ($atmosItem in $atmosXml.SelectNodes('//*[local-name()="ClCompile"][@Include]')) {
        $atmosSources += [IO.Path]::GetFullPath((Join-Path (Split-Path -Parent $atmosProjectPath) $atmosItem.Include))
    }
}
$atmosSources += Join-Path $atmosSource 'ss-atmos-capture.cpp'
$atmosObjects = @()
foreach ($atmosFile in $atmosSources) {
    $atmosObj = Join-Path $atmosOutput ([IO.Path]::GetFileNameWithoutExtension($atmosFile) + '.obj')
    Invoke-AtmosNative $atmosCompiler ($atmosFlags + @(('/Fo' + $atmosObj), $atmosFile))
    $atmosObjects += $atmosObj
}
$atmosDriver = Join-Path $atmosOutput 'soundsync-atmos-probe.sys'
$atmosLinkFlags = @('/nologo','/driver','/kernel','/subsystem:native,10.00','/entry:FxDriverEntry','/nodefaultlib',
    '/machine:x64','/guard:cf','/dynamicbase','/nxcompat','/integritycheck','/release',('/out:' + $atmosDriver),
    ('/libpath:' + "$atmosWdk/Lib/$atmosKitVersion/km/x64"),('/libpath:' + "$atmosWdk/Lib/wdf/kmdf/x64/1.15"),
    'ntoskrnl.lib','hal.lib','wmilib.lib','BufferOverflowFastFailK.lib','WdfLdr.lib','WdfDriverEntry.lib',
    'portcls.lib','stdunk.lib','libcntpr.lib','ksguid.lib','wdmsec.lib')
Invoke-AtmosNative $atmosLinker ($atmosLinkFlags + $atmosObjects)

$atmosUserFlags = @('/nologo','/std:c++17','/W4','/WX','/MT','/EHsc',('/I' + "$atmosVc/include"),
    ('/I' + "$atmosSdk/Include/$atmosKitVersion/ucrt"),('/I' + "$atmosSdk/Include/$atmosKitVersion/um"),
    ('/I' + "$atmosSdk/Include/$atmosKitVersion/shared"))
$atmosUserLink = @('/link',('/libpath:' + "$atmosVc/lib/x64"),('/libpath:' + "$atmosSdkLibraries/ucrt/x64"),('/libpath:' + "$atmosSdkLibraries/um/x64"))
foreach ($atmosProgram in @('buffer-tests','validation-tests','capture-reader')) {
    Invoke-AtmosNative $atmosCompiler ($atmosUserFlags + @((Join-Path $atmosExperiment "$atmosProgram.cpp"),
        ('/Fe' + (Join-Path $atmosOutput "$atmosProgram.exe")),('/Fo' + (Join-Path $atmosOutput "$atmosProgram.obj"))) + $atmosUserLink)
}
Invoke-AtmosNative (Join-Path $atmosOutput 'buffer-tests.exe') @()
Invoke-AtmosNative (Join-Path $atmosOutput 'validation-tests.exe') @()
Invoke-AtmosNative (Join-Path $atmosOutput 'capture-reader.exe') @('--help')
Invoke-AtmosNative cargo @('build','--manifest-path',(Join-Path $atmosExperiment 'endpoint-probe/Cargo.toml'),
    '--target-dir',(Join-Path $atmosWorkspace 'target'),'--locked')
Copy-Item -LiteralPath (Join-Path $atmosWorkspace 'target/debug/soundsync-atmos-endpoint-probe.exe') -Destination $atmosOutput
Copy-Item -LiteralPath (Join-Path $atmosExperiment 'soundsync-atmos-probe.inf') -Destination $atmosOutput
Invoke-AtmosNative (Join-Path $atmosWdk "tools/$atmosKitVersion/x64/infverif.exe") @('/w','/v',(Join-Path $atmosOutput 'soundsync-atmos-probe.inf'))
Invoke-AtmosNative (Join-Path $atmosWdk "bin/$atmosKitVersion/x86/Inf2Cat.exe") @(('/driver:' + $atmosOutput),'/os:10_NI_X64','/uselocaltime')

$atmosManifest = [ordered]@{
    status = 'UNSIGNED, NOT INSTALLED, NATIVE ATMOS UNVERIFIED'
    upstream = 'https://github.com/microsoft/Windows-driver-samples'; commit = $atmosCommit
    wdk = $atmosVersion; compiler = $atmosVc; packages = $atmosHashes
    artifacts = @(foreach ($atmosFile in @('soundsync-atmos-probe.sys','soundsync-atmos-probe.inf','soundsync-atmos-probe.cat','capture-reader.exe','soundsync-atmos-endpoint-probe.exe')) {
        [ordered]@{ name = $atmosFile; sha256 = (Get-FileHash -LiteralPath (Join-Path $atmosOutput $atmosFile) -Algorithm SHA256).Hash }
    })
}
$atmosManifest | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $atmosOutput 'manifest.json') -Encoding utf8
$atmosPackageOutput = Join-Path $atmosWorkspace 'dist/atmos-capture-probe'
New-Item -ItemType Directory -Path $atmosPackageOutput -Force | Out-Null
foreach ($atmosFile in @('soundsync-atmos-probe.sys','soundsync-atmos-probe.inf','soundsync-atmos-probe.cat','capture-reader.exe','soundsync-atmos-endpoint-probe.exe','manifest.json')) {
    Copy-Item -LiteralPath (Join-Path $atmosOutput $atmosFile) -Destination $atmosPackageOutput
}
Copy-Item -LiteralPath (Join-Path $atmosExperiment 'README.md') -Destination $atmosPackageOutput
Copy-Item -LiteralPath (Join-Path $atmosExperiment 'UPSTREAM-LICENSE.txt') -Destination $atmosPackageOutput
Compress-Archive -LiteralPath $atmosPackageOutput -DestinationPath (Join-Path $atmosWorkspace 'dist/SoundSync-Atmos-capture-probe-unsigned.zip') -Force
Write-Output "Unsigned probe and reader built and checked: $atmosOutput"
Write-Output 'No device was installed, no sound played, and no system setting changed.'
