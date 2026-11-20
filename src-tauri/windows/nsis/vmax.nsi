; VmaX NSIS Installer
; Builds a single VmaX-Setup.exe that installs prerequisites silently,
; then unpacks the app. Run from build_vmax_release.ps1 after tauri build.

!define PRODUCT_NAME "VmaX"
!define PRODUCT_VERSION "5.3.1"
!define PRODUCT_PUBLISHER "VmaX"
!define PRODUCT_WEB_SITE "https://github.com/your-repo/vmax"
!define APP_EXE "VmaX.exe"

; Include Modern UI 2
!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"
!include "FileFunc.nsh"
!include "WinCore.nsh"

; General settings
Name "${PRODUCT_NAME} ${PRODUCT_VERSION}"
OutFile "VmaX-Setup.exe"
InstallDir "$LOCALAPPDATA\${PRODUCT_NAME}"
InstallDirRegKey HKCU "Software\${PRODUCT_NAME}" ""
RequestExecutionLevel user
ShowInstDetails show

; Modern UI pages
!define MUI_ABORTWARNING
!define MUI_ICON "${NSISDIR}\Contrib\Graphics\Icons\modern-install.ico"
!define MUI_UNICON "${NSISDIR}\Contrib\Graphics\Icons\modern-uninstall.ico"
!define MUI_WELCOMEFINISHPAGE_BITMAP "${NSISDIR}\Contrib\Graphics\Wizard\win.bmp"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "${APP_EXE}"
!define MUI_FINISHPAGE_RUN_NOTCHECKED
!define MUI_FINISHPAGE_RUN_TEXT "Launch VmaX"
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_WELCOME
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_UNPAGE_FINISH
!insertmacro MUI_LANGUAGE "English"

; Reserved files (prerequisites) - downloaded at build time, embedded in installer
!define WEBVIEW2_BOOTSTRAPPER "MicrosoftEdgeWebview2Setup.exe"
!define VC_REDIST_X64 "vc_redist.x64.exe"

Section "MainSection" SEC_MAIN
    SetOutPath "$INSTDIR"

    ; --- 1. Check & install VC++ Redistributable (silent, no reboot) ---
    ${If} ${RunningX64}
        ; Detect if VC++ 2015-2022 x64 is already installed
        ClearErrors
        ReadRegStr $0 HKLM "SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64" "Version"
        IfErrors 0 VCPP_DETECTED
        ReadRegStr $0 HKLM "SOFTWARE\WOW6432Node\Microsoft\VisualStudio\14.0\VC\Runtimes\x64" "Version"
        IfErrors 0 VCPP_DETECTED
        ; Not found - install embedded redist
        File "${VC_REDIST_X64}"
        nsExec::ExecToStack '"$OUTDIR\${VC_REDIST_X64}" /install /quiet /norestart'
        Pop $0
        StrCmp $0 "success" 0 VCPP_INSTALL_FAILED
        Delete "$OUTDIR\${VC_REDIST_X64}"
        Goto VCPP_DONE
    VCPP_INSTALL_FAILED:
        MessageBox MB_ICONEXCLAMATION "VC++ Redistributable install failed (code $0). VmaX may not start."
    VCPP_DETECTED:
    VCPP_DONE:
    ${EndIf}

    ; --- 2. Check & install WebView2 Evergreen Runtime ---
    ; Detection: check if WebView2 runtime is present (Evergreen)
    ClearErrors
    ReadRegStr $0 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" "pv"
    IfErrors 0 WEBVIEW2_DETECTED
    ReadRegStr $0 HKLM "SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" "pv"
    IfErrors 0 WEBVIEW2_DETECTED
    ; Not found - install embedded bootstrapper
    File "${WEBVIEW2_BOOTSTRAPPER}"
    nsExec::ExecToStack '"$OUTDIR\${WEBVIEW2_BOOTSTRAPPER}" /silent /install'
    Pop $0
    StrCmp $0 "success" 0 WEBVIEW2_INSTALL_FAILED
    Delete "$OUTDIR\${WEBVIEW2_BOOTSTRAPPER}"
    Goto WEBVIEW2_DONE
WEBVIEW2_INSTALL_FAILED:
    MessageBox MB_ICONEXCLAMATION "WebView2 Runtime install failed (code $0). VmaX will not run without it."
WEBVIEW2_DETECTED:
WEBVIEW2_DONE:

    ; --- 3. Copy application files ---
    ; The build script stages the tauri build output here before calling makensis
    File /r "app\*"

    ; --- 4. Create uninstaller ---
    WriteUninstaller "$INSTDIR\Uninstall.exe"

    ; --- 5. Registry for detection/upgrades ---
    WriteRegStr HKCU "Software\${PRODUCT_NAME}" "" "$INSTDIR"
    WriteRegStr HKCU "Software\${PRODUCT_NAME}" "Version" "${PRODUCT_VERSION}"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" "DisplayName" "${PRODUCT_NAME}"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" "DisplayVersion" "${PRODUCT_VERSION}"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" "Publisher" "${PRODUCT_PUBLISHER}"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" "URLInfoAbout" "${PRODUCT_WEB_SITE}"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" "UninstallString" "$INSTDIR\Uninstall.exe"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" "InstallLocation" "$INSTDIR"
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" "NoModify" 1
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}" "NoRepair" 1

    ; --- 6. Start Menu shortcut ---
    CreateDirectory "$SMPROGRAMS\${PRODUCT_NAME}"
    CreateShortCut "$SMPROGRAMS\${PRODUCT_NAME}\${PRODUCT_NAME}.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\${APP_EXE}" 0
    CreateShortCut "$SMPROGRAMS\${PRODUCT_NAME}\Uninstall.lnk" "$INSTDIR\Uninstall.exe" "" "$INSTDIR\Uninstall.exe" 0

    ; --- 7. Desktop shortcut (optional, ask user) ---
    ; CreateShortCut "$DESKTOP\${PRODUCT_NAME}.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\${APP_EXE}" 0
SectionEnd

Section "Uninstall"
    ; Remove Start Menu folder
    RMDir /r "$SMPROGRAMS\${PRODUCT_NAME}"

    ; Remove app files
    RMDir /r "$INSTDIR"

    ; Remove registry keys
    DeleteRegKey HKCU "Software\${PRODUCT_NAME}"
    DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCT_NAME}"
SectionEnd

; Function to launch app on finish page
Function LaunchApp
    Exec '"$INSTDIR\${APP_EXE}"'
FunctionEnd