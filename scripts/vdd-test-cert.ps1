# Make the development certificate the display driver is signed with, and make
# this machine trust it: LocalMachine Root and TrustedPublisher. A user-mode
# driver needs nothing more -- no test mode, whatever the state of Secure Boot
# (docs/11-drivers.md section 1). The private key stays in the developer's own
# store and cannot be exported, so packaging needs no elevation; trusting the
# certificate does. Run once per machine from an elevated shell of the
# developer's account; it prints the thumbprint scripts/package-vdd.sh takes.
# A second run reuses the certificate.

$ErrorActionPreference = 'Stop'
$subject = 'CN=lowlat development driver signing'
$cert = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq $subject } |
    Sort-Object NotAfter -Descending | Select-Object -First 1
if (-not $cert) {
    $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject $subject `
        -KeyAlgorithm RSA -KeyLength 3072 -HashAlgorithm SHA256 -KeyUsage DigitalSignature `
        -KeyExportPolicy NonExportable -CertStoreLocation Cert:\CurrentUser\My `
        -NotAfter (Get-Date).AddYears(5)
}
$public = Join-Path ([IO.Path]::GetTempPath()) "lowlat-vdd-$($cert.Thumbprint).cer"
Export-Certificate -Cert $cert -FilePath $public | Out-Null
foreach ($store in 'Root', 'TrustedPublisher') {
    Import-Certificate -FilePath $public -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null
}
Remove-Item $public
$cert.Thumbprint
