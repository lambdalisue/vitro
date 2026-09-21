//! The answer file and first-logon script a Windows guest installs itself from.
//!
//! Both are carried in the binary rather than read from disk, because they are
//! not configuration: every value in them is either forced by the QEMU machine
//! vitro builds or filled in from the image's own settings. A user who genuinely
//! needs a different one points `unattend` at their own file, and vitro uses it
//! whole — Windows releases differ enough that meeting them halfway with a
//! partial template is worse than not trying.

use anyhow::{bail, Result};

/// Where the drivers and the first-logon script sit while Setup runs.
///
/// WinPE gives the seed volume `C:` — the target disk has no partitions yet, so
/// it takes no letter — and this is stable for the topology vitro builds. The
/// installed system sees the same volume at some later letter, which is why the
/// first-logon command searches for the script instead of naming a drive.
const SEED_IN_WINPE: &str = r"C:\";

/// The generic Windows 11 Pro key. It selects an edition and activates nothing.
const EDITION_KEY: &str = "VK7JG-NPHTM-C97JM-9MPGT-3V66T";

/// The architecture the installation media is for, spelled the several ways
/// Microsoft's tooling spells it.
///
/// Both spellings have to agree with the media. Setup ignores an answer file
/// whose architecture does not match, and a driver lifted from the wrong
/// directory is one Setup cannot load — neither says so, so a mismatch shows up
/// as an installer sitting at an interactive prompt half an hour in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Aarch64,
    X86_64,
}

impl Arch {
    /// What the host this is running on can install without emulation.
    ///
    /// vitro only runs accelerated same-architecture guests, so the host's
    /// architecture is the guest's. Cross-architecture guests would have to
    /// carry their own answer, which is why this is fallible rather than a
    /// default.
    pub fn of_host() -> Option<Self> {
        Self::named(std::env::consts::ARCH)
    }

    pub fn named(arch: &str) -> Option<Self> {
        match arch {
            "aarch64" => Some(Self::Aarch64),
            "x86_64" => Some(Self::X86_64),
            _ => None,
        }
    }

    /// The `processorArchitecture` attribute in the answer file.
    pub fn processor_architecture(self) -> &'static str {
        match self {
            Self::Aarch64 => "arm64",
            Self::X86_64 => "amd64",
        }
    }

    /// The directory each driver sits in on the virtio-win image.
    pub fn virtio_directory(self) -> &'static str {
        match self {
            Self::Aarch64 => "ARM64",
            Self::X86_64 => "amd64",
        }
    }
}

/// The architecture a Windows installer ISO is for, read off its label.
///
/// The installer's own files live in a UDF filesystem, and the ISO9660 side of
/// the image holds almost nothing — but the volume identifier is there, and
/// Microsoft's media names the architecture in it:
/// `CCCOMA_A64FRE_EN-US_DV9`, `CCCOMA_X64FRE_EN-US_DV9`.
///
/// Worth reading because the alternative way to find out is to start the
/// install: Setup ignores an answer file whose architecture does not match in
/// exactly the way it ignores a missing one, so the wrong ISO does not fail,
/// it sits at its first screen until somebody looks.
pub fn arch_from_volume_id(volume_id: &str) -> Option<Arch> {
    let upper = volume_id.to_ascii_uppercase();
    // Underscore-delimited so `X64` does not match inside some longer word.
    let names = upper.split(['_', '-']).collect::<Vec<_>>();
    if names.iter().any(|part| part.starts_with("A64")) {
        return Some(Arch::Aarch64);
    }
    if names.iter().any(|part| part.starts_with("X64")) {
        return Some(Arch::X86_64);
    }
    None
}

pub struct Answers {
    pub user: String,
    pub password: String,
    pub computer_name: String,
    /// The `/IMAGE/NAME` to install, as the media spells it.
    pub edition: String,
    /// The architecture of the media the answer file is going onto.
    pub arch: Arch,
}

impl Default for Answers {
    fn default() -> Self {
        Self {
            user: "vitro".into(),
            password: String::new(),
            computer_name: "vitro-win".into(),
            edition: "Windows 11 Pro".into(),
            arch: Arch::Aarch64,
        }
    }
}

/// Build the answer file.
///
/// Values are XML-escaped on the way in. A password containing `&` would
/// otherwise produce a file Setup silently ignores, which is indistinguishable
/// from no answer file at all.
pub fn answer_file(answers: &Answers) -> Result<String> {
    if answers.user.is_empty() || answers.password.is_empty() {
        bail!("the Windows account needs a name and a password");
    }
    Ok(TEMPLATE
        .replace("{{ARCH}}", answers.arch.processor_architecture())
        .replace("{{DRIVER_PATH}}", &escape(SEED_IN_WINPE))
        .replace("{{EDITION}}", &escape(&answers.edition))
        .replace("{{EDITION_KEY}}", EDITION_KEY)
        .replace("{{COMPUTER_NAME}}", &escape(&answers.computer_name))
        .replace("{{USER}}", &escape(&answers.user))
        .replace("{{PASSWORD}}", &escape(&answers.password)))
}

/// The first-logon script, with the account it should keep logged in.
pub fn setup_script(answers: &Answers) -> String {
    SETUP_SCRIPT
        .replace("{{USER}}", &answers.user)
        .replace("{{PASSWORD}}", &answers.password)
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// `processorArchitecture` is substituted rather than fixed: Setup ignores an
/// answer file whose architecture does not match the media, and the failure is
/// indistinguishable from there being no answer file at all.
const TEMPLATE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend">
  <settings pass="windowsPE">
    <component name="Microsoft-Windows-International-Core-WinPE"
               processorArchitecture="{{ARCH}}"
               publicKeyToken="31bf3856ad364e35" language="neutral"
               versionScope="nonSxS"
               xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <SetupUILanguage>
        <UILanguage>en-US</UILanguage>
      </SetupUILanguage>
      <InputLocale>0409:00000409</InputLocale>
      <SystemLocale>en-US</SystemLocale>
      <UILanguage>en-US</UILanguage>
      <UserLocale>en-US</UserLocale>
    </component>

    <component name="Microsoft-Windows-PnpCustomizationsWinPE"
               processorArchitecture="{{ARCH}}"
               publicKeyToken="31bf3856ad364e35" language="neutral"
               versionScope="nonSxS"
               xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <!--
        Exactly one path, and it must exist. A DriverPaths entry pointing at a
        drive that is not there does not get skipped: it fails the whole
        windowsPE pass with 0xD000A000 - 0x40031, before any log exists to say
        so.
      -->
      <DriverPaths>
        <PathAndCredentials wcm:action="add" wcm:keyValue="1">
          <Path>{{DRIVER_PATH}}</Path>
        </PathAndCredentials>
      </DriverPaths>
    </component>

    <component name="Microsoft-Windows-Setup"
               processorArchitecture="{{ARCH}}"
               publicKeyToken="31bf3856ad364e35" language="neutral"
               versionScope="nonSxS"
               xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <!--
        Windows 11 refuses to install without TPM 2.0, Secure Boot and enough
        RAM. The QEMU virt machine offers none of the first two, so Setup's own
        bypass switches go in before the compatibility check runs.
      -->
      <RunSynchronous>
        <RunSynchronousCommand wcm:action="add">
          <Order>1</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassTPMCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>2</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassSecureBootCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>3</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassRAMCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>4</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassCPUCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
        <RunSynchronousCommand wcm:action="add">
          <Order>5</Order>
          <Path>reg add HKLM\SYSTEM\Setup\LabConfig /v BypassStorageCheck /t REG_DWORD /d 1 /f</Path>
        </RunSynchronousCommand>
      </RunSynchronous>

      <!--
        DiskID is the enumeration order of that boot, not a name. The inbox USB
        storage driver comes up before viostor is read from DriverPaths, so the
        seed volume is disk 0 and the virtio target is disk 1. Getting this
        wrong wipes the volume holding this file.
      -->
      <DiskConfiguration>
        <WillShowUI>OnError</WillShowUI>
        <Disk wcm:action="add">
          <DiskID>1</DiskID>
          <WillWipeDisk>true</WillWipeDisk>
          <CreatePartitions>
            <CreatePartition wcm:action="add">
              <Order>1</Order>
              <Type>EFI</Type>
              <Size>300</Size>
            </CreatePartition>
            <CreatePartition wcm:action="add">
              <Order>2</Order>
              <Type>MSR</Type>
              <Size>16</Size>
            </CreatePartition>
            <CreatePartition wcm:action="add">
              <Order>3</Order>
              <Type>Primary</Type>
              <Extend>true</Extend>
            </CreatePartition>
          </CreatePartitions>
          <ModifyPartitions>
            <ModifyPartition wcm:action="add">
              <Order>1</Order>
              <PartitionID>1</PartitionID>
              <Format>FAT32</Format>
              <Label>System</Label>
            </ModifyPartition>
            <ModifyPartition wcm:action="add">
              <Order>2</Order>
              <PartitionID>2</PartitionID>
            </ModifyPartition>
            <ModifyPartition wcm:action="add">
              <Order>3</Order>
              <PartitionID>3</PartitionID>
              <Format>NTFS</Format>
              <Label>Windows</Label>
              <Letter>C</Letter>
            </ModifyPartition>
          </ModifyPartitions>
        </Disk>
      </DiskConfiguration>

      <ImageInstall>
        <OSImage>
          <InstallFrom>
            <MetaData wcm:action="add">
              <Key>/IMAGE/NAME</Key>
              <Value>{{EDITION}}</Value>
            </MetaData>
          </InstallFrom>
          <InstallTo>
            <DiskID>1</DiskID>
            <PartitionID>3</PartitionID>
          </InstallTo>
          <WillShowUI>OnError</WillShowUI>
        </OSImage>
      </ImageInstall>

      <UserData>
        <ProductKey>
          <Key>{{EDITION_KEY}}</Key>
        </ProductKey>
        <AcceptEula>true</AcceptEula>
      </UserData>
    </component>
  </settings>

  <settings pass="specialize">
    <component name="Microsoft-Windows-Shell-Setup"
               processorArchitecture="{{ARCH}}"
               publicKeyToken="31bf3856ad364e35" language="neutral"
               versionScope="nonSxS"
               xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <!-- Without this the name is random, and so is the guest's host key. -->
      <ComputerName>{{COMPUTER_NAME}}</ComputerName>
    </component>
  </settings>

  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-Shell-Setup"
               processorArchitecture="{{ARCH}}"
               publicKeyToken="31bf3856ad364e35" language="neutral"
               versionScope="nonSxS"
               xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <OOBE>
        <HideEULAPage>true</HideEULAPage>
        <HideLocalAccountScreen>true</HideLocalAccountScreen>
        <HideOEMRegistrationScreen>true</HideOEMRegistrationScreen>
        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>
        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
        <ProtectYourPC>3</ProtectYourPC>
        <SkipMachineOOBE>true</SkipMachineOOBE>
        <SkipUserOOBE>true</SkipUserOOBE>
      </OOBE>
      <UserAccounts>
        <LocalAccounts>
          <LocalAccount wcm:action="add">
            <Name>{{USER}}</Name>
            <Group>Administrators</Group>
            <Password>
              <Value>{{PASSWORD}}</Value>
              <PlainText>true</PlainText>
            </Password>
          </LocalAccount>
        </LocalAccounts>
      </UserAccounts>
      <AutoLogon>
        <Username>{{USER}}</Username>
        <Password>
          <Value>{{PASSWORD}}</Value>
          <PlainText>true</PlainText>
        </Password>
        <Enabled>true</Enabled>
        <LogonCount>1</LogonCount>
      </AutoLogon>
      <FirstLogonCommands>
        <SynchronousCommand wcm:action="add">
          <Order>1</Order>
          <Description>Install the virtio drivers, then OpenSSH</Description>
          <CommandLine>powershell -NoProfile -ExecutionPolicy Bypass -Command "foreach($d in 'D','E','F','G','H'){ $p=$d+':\vitro-setup.ps1'; if(Test-Path $p){ &amp; $p; break } }"</CommandLine>
        </SynchronousCommand>
      </FirstLogonCommands>
    </component>
  </settings>
</unattend>
"#;

/// Runs at first logon from whichever drive letter the seed volume landed on.
///
/// The order is not negotiable. Windows on ARM has no inbox virtio driver, so
/// the guest has no network until NetKVM is installed — and OpenSSH Server is a
/// Feature on Demand, fetched over that network.
const SETUP_SCRIPT: &str = r#"$ErrorActionPreference = 'Continue'
$log = 'C:\vitro-setup.log'
function Note($m) { "$(Get-Date -Format o)  $m" | Tee-Object -FilePath $log -Append }

$here = Split-Path -Parent $PSCommandPath
Note "starting, script at $PSCommandPath"

# Every .inf next to this script, not a chosen few: netkvm.inf copies
# netkvmp.exe as well, and pnputil reports only "cannot find the file
# specified" when something it needs is absent.
foreach ($inf in Get-ChildItem -Path $here -Filter *.inf -File) {
    $out = & pnputil /add-driver $inf.FullName /install 2>&1
    Note "pnputil $($inf.Name): $($out -join ' / ')"
}

# The NIC needs a moment to appear and take a DHCP lease from QEMU's NAT.
$online = $false
foreach ($i in 1..30) {
    if (Test-NetConnection -ComputerName '10.0.2.2' -InformationLevel Quiet -WarningAction SilentlyContinue) {
        $online = $true
        break
    }
    Start-Sleep -Seconds 2
}
Note "network reachable: $online"

Note "adding OpenSSH server"
$cap = Add-WindowsCapability -Online -Name 'OpenSSH.Server~~~~0.0.1.0' 2>&1
Note ($cap | Out-String)

# Installing OpenSSH is not enough. The rule it creates, OpenSSH-Server-In-TCP,
# is scoped to the Private profile, while a fresh guest classifies QEMU's NAT as
# Public — so the rule never applies and the host's SSH hangs waiting for a
# banner. Classify the connection rather than weakening the firewall.
Get-NetConnectionProfile | ForEach-Object {
    Set-NetConnectionProfile -InterfaceIndex $_.InterfaceIndex -NetworkCategory Private
}
Note "network category: $((Get-NetConnectionProfile).NetworkCategory -join ',')"

Set-Service -Name sshd -StartupType Automatic -ErrorAction SilentlyContinue
Start-Service -Name sshd -ErrorAction SilentlyContinue
Note "sshd status: $((Get-Service sshd -ErrorAction SilentlyContinue).Status)"

# An administrator's keys live in one machine-wide file, not in the profile, and
# sshd refuses it unless only SYSTEM and Administrators can write it.
$pub = Join-Path $here 'authorized_keys'
if (Test-Path $pub) {
    $dst = 'C:\ProgramData\ssh\administrators_authorized_keys'
    Copy-Item $pub $dst -Force
    icacls $dst /inheritance:r /grant 'SYSTEM:F' /grant 'BUILTIN\Administrators:F' | Out-Null
    Note "installed authorized_keys"
} else {
    Note "no authorized_keys on the seed volume"
}

# The answer file's AutoLogon spends its LogonCount on this very boot, so the
# next one would stop at the logon screen. A GUI program can only be shown on a
# desktop somebody is logged in to, so the golden image needs the permanent
# form, which lives in Winlogon's registry key.
$winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
Set-ItemProperty -Path $winlogon -Name AutoAdminLogon -Value '1' -Type String
Set-ItemProperty -Path $winlogon -Name DefaultUserName -Value '{{USER}}' -Type String
Set-ItemProperty -Path $winlogon -Name DefaultPassword -Value '{{PASSWORD}}' -Type String
# AutoLogonCount outranks AutoAdminLogon, and Winlogon does not merely stop
# using it when it reaches zero: it deletes DefaultPassword and writes
# AutoAdminLogon back to 0. The answer file's <LogonCount> put a count here, so
# leaving it means the three lines above are erased soon after they are
# written, and the golden image boots to the logon screen with no way in.
Remove-ItemProperty -Path $winlogon -Name AutoLogonCount -ErrorAction SilentlyContinue
Note "permanent auto-logon set for {{USER}}"

# A logged-in desktop is no use if the display has been switched off: the
# monitor keeps reporting a framebuffer, so `screenshot` captures a black
# rectangle and nothing says why. Sleeping the machine is worse still, because
# ssh stops answering too. None of the three timeouts mean anything to a guest
# nobody sits in front of.
foreach ($timeout in 'monitor-timeout-ac', 'monitor-timeout-dc',
                     'standby-timeout-ac', 'standby-timeout-dc',
                     'disk-timeout-ac', 'disk-timeout-dc') {
    powercfg /change $timeout 0 | Out-Null
}
Note "display and sleep timeouts disabled"

# An OpenSSH session on Windows runs cmd.exe unless told otherwise, and vitro
# quotes the arguments it sends with single quotes — which cmd does not treat
# as quoting at all. Fixing the shell here is what lets one quoting rule serve
# every guest, and PowerShell is the one that agrees with it.
New-ItemProperty -Path 'HKLM:\SOFTWARE\OpenSSH' -Name DefaultShell `
    -Value 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe' `
    -PropertyType String -Force | Out-Null

'unattend-complete' | Out-File -FilePath 'C:\vitro-installed.txt' -Encoding ascii
Note "done"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn answers() -> Answers {
        Answers {
            password: "s3cret".into(),
            ..Answers::default()
        }
    }

    #[test]
    fn every_placeholder_is_filled_in() {
        let xml = answer_file(&answers()).unwrap();

        assert!(!xml.contains("{{"), "a placeholder survived:\n{xml}");
    }

    #[test]
    fn the_account_and_the_machine_name_come_from_the_caller() {
        let xml = answer_file(&Answers {
            user: "dev".into(),
            password: "s3cret".into(),
            computer_name: "buildbox".into(),
            ..Answers::default()
        })
        .unwrap();

        assert!(xml.contains("<Name>dev</Name>"), "{xml}");
        assert!(xml.contains("<Username>dev</Username>"), "{xml}");
        assert!(
            xml.contains("<ComputerName>buildbox</ComputerName>"),
            "{xml}"
        );
        assert!(xml.contains("<Value>s3cret</Value>"), "{xml}");
    }

    #[test]
    fn a_value_with_xml_in_it_does_not_break_the_file() {
        // Setup ignores a malformed answer file exactly the way it ignores a
        // missing one, so this would surface as "the install stopped at the
        // first screen" half an hour later.
        let xml = answer_file(&Answers {
            password: "a&b<c>\"d\"".into(),
            ..Answers::default()
        })
        .unwrap();

        assert!(
            xml.contains("<Value>a&amp;b&lt;c&gt;&quot;d&quot;</Value>"),
            "{xml}"
        );
        assert!(!xml.contains("a&b<c>"), "{xml}");
    }

    #[test]
    fn an_account_without_a_password_is_refused_before_a_long_install_starts() {
        let err = answer_file(&Answers::default()).unwrap_err().to_string();

        assert!(err.contains("password"), "{err}");
    }

    #[test]
    fn every_component_names_the_architecture_of_the_media() {
        // One component left on the wrong architecture is enough: Setup takes
        // the file as not applying to its media and installs interactively,
        // which on a headless build is a hang rather than an error.
        for (arch, expected, wrong) in [
            (Arch::Aarch64, "arm64", "amd64"),
            (Arch::X86_64, "amd64", "arm64"),
        ] {
            let xml = answer_file(&Answers { arch, ..answers() }).unwrap();

            let components = xml.matches("<component ").count();
            assert_eq!(
                xml.matches(&format!("processorArchitecture=\"{expected}\""))
                    .count(),
                components,
                "{arch:?} left a component behind: {xml}"
            );
            assert!(
                !xml.contains(&format!("processorArchitecture=\"{wrong}\"")),
                "{arch:?} kept the other architecture: {xml}"
            );
            assert!(!xml.contains("{{ARCH}}"), "a placeholder survived: {xml}");
        }
    }

    #[test]
    fn the_architecture_is_read_off_the_media_label() {
        // Real labels from Microsoft's own downloads.
        assert_eq!(
            arch_from_volume_id("CCCOMA_A64FRE_EN-US_DV9"),
            Some(Arch::Aarch64)
        );
        assert_eq!(
            arch_from_volume_id("CCCOMA_X64FRE_EN-US_DV9"),
            Some(Arch::X86_64)
        );
    }

    #[test]
    fn a_label_that_names_no_architecture_is_left_alone() {
        // Anything vitro cannot read confidently has to pass rather than be
        // guessed at: refusing somebody's perfectly good custom media would be
        // worse than not checking it.
        assert_eq!(arch_from_volume_id("virtio-win-0.1.302"), None);
        assert_eq!(arch_from_volume_id(""), None);
        assert_eq!(arch_from_volume_id("MY_WINDOWS_MEDIA"), None);
    }

    #[test]
    fn an_architecture_the_host_does_not_have_is_not_guessed_at() {
        // The guest's architecture is the host's, so an unknown host has to
        // stop the build rather than pick one and fail half an hour later.
        assert_eq!(Arch::named("aarch64"), Some(Arch::Aarch64));
        assert_eq!(Arch::named("x86_64"), Some(Arch::X86_64));
        assert_eq!(Arch::named("riscv64"), None);
    }

    #[test]
    fn the_seed_volume_is_the_only_driver_path() {
        // More than one, or one that is not there, fails the whole windowsPE
        // pass rather than being skipped.
        let xml = answer_file(&answers()).unwrap();

        assert_eq!(xml.matches("<PathAndCredentials").count(), 1, "{xml}");
        assert!(xml.contains(r"<Path>C:\</Path>"), "{xml}");
    }

    #[test]
    fn the_install_target_is_the_second_disk_not_the_seed() {
        let xml = answer_file(&answers()).unwrap();

        assert!(!xml.contains("<DiskID>0</DiskID>"), "{xml}");
        assert_eq!(xml.matches("<DiskID>1</DiskID>").count(), 2, "{xml}");
    }

    #[test]
    fn the_first_logon_command_searches_for_the_script_instead_of_naming_a_drive() {
        // The seed is C: in WinPE and something else once Windows is installed.
        let xml = answer_file(&answers()).unwrap();

        assert!(xml.contains("vitro-setup.ps1"), "{xml}");
        assert!(xml.contains("Test-Path"), "{xml}");
    }

    #[test]
    fn the_guest_is_left_with_a_shell_that_quotes_the_way_vitro_does() {
        // vitro sends single-quoted arguments; cmd.exe would pass the quotes
        // through as part of the argument.
        let script = setup_script(&answers());

        assert!(script.contains("DefaultShell"), "{script}");
        assert!(script.contains("powershell.exe"), "{script}");
    }

    #[test]
    fn the_guest_keeps_logging_itself_in_after_the_answer_file_stops_doing_it() {
        // Without this the second boot stops at the logon screen, and nothing
        // can be shown on a desktop that nobody is logged in to.
        let script = setup_script(&Answers {
            user: "dev".into(),
            password: "s3cret".into(),
            ..Answers::default()
        });

        assert!(script.contains("AutoAdminLogon"), "{script}");
        assert!(script.contains("-Value 'dev'"), "{script}");
        assert!(script.contains("-Value 's3cret'"), "{script}");
        assert!(!script.contains("{{"), "a placeholder survived");

        // Setting AutoAdminLogon is not enough on its own. Winlogon treats a
        // leftover AutoLogonCount as the authority, and on reaching zero it
        // deletes DefaultPassword and puts AutoAdminLogon back to 0 — so a
        // script that only writes the three values above has them erased and
        // the guest boots to the logon screen.
        assert!(
            script.contains("Remove-ItemProperty") && script.contains("AutoLogonCount"),
            "the answer file's logon count has to go, or the permanent form is undone: {script}"
        );
    }

    #[test]
    fn the_guest_never_switches_its_display_off() {
        // A blanked display photographs as a black rectangle, so `screenshot`
        // would report a working GUI as a failure and a broken one the same
        // way. Nobody is sitting in front of this machine to wake it.
        let script = setup_script(&Answers::default());

        assert!(script.contains("monitor-timeout-ac"), "{script}");
        assert!(script.contains("standby-timeout-ac"), "{script}");
    }

    #[test]
    fn the_setup_script_installs_the_network_before_it_needs_the_network() {
        let script = setup_script(&answers());
        let pnputil = script.find("pnputil").expect("drivers");
        let capability = script.find("Add-WindowsCapability").expect("OpenSSH");

        assert!(
            pnputil < capability,
            "OpenSSH is fetched over the network the driver provides"
        );
    }
}
