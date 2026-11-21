; Stitch Windows Installer Script (NSIS)
; One-click per-user installation matching Squirrel / modern Electron behavior.

!include "MUI2.nsh"
!include "FileFunc.nsh"

; --------------------------------------------------
; General Definitions
; --------------------------------------------------
!define PRODUCT_NAME "Stitch"
!define PRODUCT_PUBLISHER "Stitch"
!define APP_DIR_NAME "Stitch"
!define EXECUTABLE_NAME "stitch-rs.exe"

!ifndef VERSION
  !define VERSION "0.1.0"
!endif

!ifndef VI_VERSION
  !define VI_VERSION "${VERSION}.0"
!endif

!ifndef ICON_PATH
  !define ICON_PATH "assets\icon.ico"
!endif

!ifndef EXE_PATH
  !define EXE_PATH "target\x86_64-pc-windows-msvc\release\stitch-rs.exe"
!endif

!ifndef RUNTIME_DIR
  !define RUNTIME_DIR "target\copilot-runtime"
!endif

!ifndef OUTPUT_FILE
  !define OUTPUT_FILE "target\x86_64-pc-windows-msvc\release\Stitch-Setup.exe"
!endif

; --------------------------------------------------
; Installer Settings
; --------------------------------------------------
Name "${PRODUCT_NAME}"
OutFile "${OUTPUT_FILE}"
InstallDir "$LOCALAPPDATA\Programs\${APP_DIR_NAME}"
RequestExecutionLevel user
SetCompressor /SOLID lzma

; Icons
!define MUI_ICON "${ICON_PATH}"
!define MUI_UNICON "${ICON_PATH}"

; Version Information
VIProductVersion "${VI_VERSION}"
VIAddVersionKey "ProductName" "${PRODUCT_NAME}"
VIAddVersionKey "CompanyName" "${PRODUCT_PUBLISHER}"
VIAddVersionKey "LegalCopyright" "${PRODUCT_PUBLISHER}"
VIAddVersionKey "FileDescription" "${PRODUCT_NAME} Setup"
VIAddVersionKey "FileVersion" "${VI_VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"

; --------------------------------------------------
; Modern UI Settings (One-Click / Minimal UI)
; --------------------------------------------------
!define MUI_ABORTWARNING
AutoCloseWindow true
ShowInstDetails nevershow

!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; --------------------------------------------------
; Installer Section
; --------------------------------------------------
Section "MainSection" SEC01
  ; Terminate running instance and any child Copilot runtime processes before replacing files
  nsExec::Exec 'taskkill /F /T /IM ${EXECUTABLE_NAME}'

  ; Set output destination and copy executable and icon
  SetOutPath "$INSTDIR"
  File "/oname=${EXECUTABLE_NAME}" "${EXE_PATH}"
  File "/oname=icon.ico" "${ICON_PATH}"

  ; Replace bundled Copilot runtime cleanly on upgrades
  RMDir /r "$INSTDIR\copilot-runtime"
  SetOutPath "$INSTDIR\copilot-runtime"
  File /r /x "*.staging-*" /x ".hostless-runtime-assets-v1" "${RUNTIME_DIR}\*"
  SetOutPath "$INSTDIR"

  ; Write uninstaller
  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; Create Start Menu shortcut (No desktop shortcut as configured)
  CreateDirectory "$SMPROGRAMS\${PRODUCT_NAME}"
  CreateShortcut "$SMPROGRAMS\${PRODUCT_NAME}\${PRODUCT_NAME}.lnk" "$INSTDIR\${EXECUTABLE_NAME}" "" "$INSTDIR\icon.ico" 0

  ; Register in Windows Add/Remove Programs (HKCU per-user)
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_DIR_NAME}" "DisplayName" "${PRODUCT_NAME}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_DIR_NAME}" "DisplayIcon" "$INSTDIR\icon.ico,0"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_DIR_NAME}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_DIR_NAME}" "Publisher" "${PRODUCT_PUBLISHER}"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_DIR_NAME}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_DIR_NAME}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_DIR_NAME}" "InstallLocation" "$INSTDIR"
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_DIR_NAME}" "NoModify" 1
  WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_DIR_NAME}" "NoRepair" 1

  ; Immediately launch the installed application
  Exec '"$INSTDIR\${EXECUTABLE_NAME}"'
SectionEnd

; --------------------------------------------------
; Uninstaller Section
; --------------------------------------------------
Section "Uninstall"
  ; Terminate running instance and any child Copilot runtime processes before deleting files
  nsExec::Exec 'taskkill /F /T /IM ${EXECUTABLE_NAME}'

  ; Remove Start Menu shortcut and folder
  Delete "$SMPROGRAMS\${PRODUCT_NAME}\${PRODUCT_NAME}.lnk"
  RMDir "$SMPROGRAMS\${PRODUCT_NAME}"

  ; Remove application files
  Delete "$INSTDIR\${EXECUTABLE_NAME}"
  Delete "$INSTDIR\icon.ico"
  Delete "$INSTDIR\uninstall.exe"
  RMDir /r "$INSTDIR\copilot-runtime"
  RMDir "$INSTDIR"

  ; Remove Add/Remove Programs registry key
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_DIR_NAME}"
SectionEnd
