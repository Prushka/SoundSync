[CmdletBinding()]
param([string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
$atmosWorkspace = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$atmosUnsigned = Join-Path $atmosWorkspace 'dist/atmos-capture-probe'
if (!$OutputDirectory) { $OutputDirectory = Join-Path $atmosWorkspace 'dist/atmos-capture-test' }
$atmosSigned = [IO.Path]::GetFullPath($OutputDirectory)
$atmosSourceManifest = Get-Content -LiteralPath (Join-Path $atmosUnsigned 'manifest.json') -Raw | ConvertFrom-Json
foreach ($atmosArtifact in $atmosSourceManifest.artifacts) {
    if ((Get-FileHash -LiteralPath (Join-Path $atmosUnsigned $atmosArtifact.name) -Algorithm SHA256).Hash -ne $atmosArtifact.sha256) {
        throw "Unsigned artifact changed: $($atmosArtifact.name). Rebuild and review it before test signing."
    }
}
if (Test-Path -LiteralPath $atmosSigned) { throw 'Test package directory already exists; do not replace a package with a pending test or rollback' }
New-Item -ItemType Directory -Path $atmosSigned | Out-Null
foreach ($atmosFile in @('soundsync-atmos-probe.sys','soundsync-atmos-probe.inf','capture-reader.exe','soundsync-atmos-endpoint-probe.exe','README.md','UPSTREAM-LICENSE.txt')) {
    Copy-Item -LiteralPath (Join-Path $atmosUnsigned $atmosFile) -Destination $atmosSigned
}
$atmosPackages = Join-Path $atmosWorkspace 'target/atmos-driver-source/packages'
$atmosSignTool = Join-Path $atmosPackages 'Microsoft.Windows.SDK.CPP.10.0.28000.2526/c/bin/10.0.28000.0/x64/signtool.exe'
$atmosInf2Cat = Join-Path $atmosPackages 'Microsoft.Windows.WDK.x64.10.0.28000.2526/c/bin/10.0.28000.0/x86/Inf2Cat.exe'
$atmosKeyFile = Join-Path $atmosWorkspace ('target/atmos-test-key-' + [Guid]::NewGuid().ToString('N') + '.pfx')
$atmosRsa = [Security.Cryptography.RSA]::Create(3072)
$atmosRequest = [Security.Cryptography.X509Certificates.CertificateRequest]::new(
    'CN=SoundSync Atmos Probe LOCAL TEST ONLY', $atmosRsa, [Security.Cryptography.HashAlgorithmName]::SHA256,
    [Security.Cryptography.RSASignaturePadding]::Pkcs1)
$atmosEku = [Security.Cryptography.OidCollection]::new()
[void]$atmosEku.Add([Security.Cryptography.Oid]::new('1.3.6.1.5.5.7.3.3'))
$atmosRequest.CertificateExtensions.Add([Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension]::new($atmosEku, $true))
$atmosRequest.CertificateExtensions.Add([Security.Cryptography.X509Certificates.X509KeyUsageExtension]::new([Security.Cryptography.X509Certificates.X509KeyUsageFlags]::DigitalSignature, $true))
$atmosRequest.CertificateExtensions.Add([Security.Cryptography.X509Certificates.X509BasicConstraintsExtension]::new($false, $false, 0, $true))
$atmosCertificate = $atmosRequest.CreateSelfSigned([DateTimeOffset]::UtcNow.AddMinutes(-5), [DateTimeOffset]::UtcNow.AddDays(7))
try {
    [IO.File]::WriteAllBytes($atmosKeyFile, $atmosCertificate.Export([Security.Cryptography.X509Certificates.X509ContentType]::Pfx, ''))
    $atmosAcl = [Security.AccessControl.FileSecurity]::new()
    $atmosAcl.SetAccessRuleProtection($true, $false)
    $atmosAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.WindowsIdentity]::GetCurrent().User, 'FullControl', 'Allow'))
    Set-Acl -LiteralPath $atmosKeyFile -AclObject $atmosAcl
    & $atmosSignTool sign /fd SHA256 /f $atmosKeyFile (Join-Path $atmosSigned 'soundsync-atmos-probe.sys')
    if ($LASTEXITCODE -ne 0) { throw 'Driver test signing failed' }
    & $atmosInf2Cat ("/driver:$atmosSigned") /os:10_NI_X64 /uselocaltime
    if ($LASTEXITCODE -ne 0) { throw 'Test catalog generation failed' }
    & $atmosSignTool sign /fd SHA256 /f $atmosKeyFile (Join-Path $atmosSigned 'soundsync-atmos-probe.cat')
    if ($LASTEXITCODE -ne 0) { throw 'Catalog test signing failed' }
    [IO.File]::WriteAllBytes((Join-Path $atmosSigned 'SoundSyncAtmosProbe-Test.cer'), $atmosCertificate.Export([Security.Cryptography.X509Certificates.X509ContentType]::Cert))
    foreach ($atmosFile in @('soundsync-atmos-probe.sys','soundsync-atmos-probe.cat')) {
        $atmosSignature = Get-AuthenticodeSignature -LiteralPath (Join-Path $atmosSigned $atmosFile)
        if ($atmosSignature.SignerCertificate.Thumbprint -ne $atmosCertificate.Thumbprint) { throw 'Unexpected signer on test package' }
    }
    $atmosManifest = [ordered]@{
        status = 'TEST-SIGNED ONLY; NOT TRUSTED OR INSTALLED'
        hardware_id = 'Root\SoundSyncAtmosProbe'
        certificate_thumbprint = $atmosCertificate.Thumbprint
        certificate_expires_utc = $atmosCertificate.NotAfter.ToUniversalTime().ToString('o')
        source_commit = $atmosSourceManifest.commit
        artifacts = @(foreach ($atmosFile in @('soundsync-atmos-probe.sys','soundsync-atmos-probe.inf','soundsync-atmos-probe.cat','capture-reader.exe','soundsync-atmos-endpoint-probe.exe','SoundSyncAtmosProbe-Test.cer')) {
            [ordered]@{ name = $atmosFile; sha256 = (Get-FileHash -LiteralPath (Join-Path $atmosSigned $atmosFile) -Algorithm SHA256).Hash }
        })
    }
    $atmosManifest | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $atmosSigned 'test-manifest.json') -Encoding utf8
} finally {
    if (Test-Path -LiteralPath $atmosKeyFile) { Remove-Item -LiteralPath $atmosKeyFile -Force }
    $atmosCertificate.Dispose()
    $atmosRsa.Dispose()
}
Write-Output "Test-signed package: $atmosSigned"
Write-Output 'No certificate was trusted, no driver installed, and no boot setting changed. The temporary private key was deleted.'
