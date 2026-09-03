; LynxRDP Windows installer.
;
; Built by packaging/make-setup-exe.sh, which passes VERSION, SOURCE and
; OUTFILE on the command line. Nothing here is signed, so Windows will warn
; about an unknown publisher; docs/INSTALL.md says what to expect.

Unicode true
ManifestDPIAware true

!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"

!insertmacro GetSize

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef EXEPATH
  !error "EXEPATH (the full path of the built lynxrdp.exe) must be defined"
!endif
!ifndef OUTFILE
  !define OUTFILE "lynxrdp-setup.exe"
!endif

!define APPNAME "LynxRDP"
!define PUBLISHER "LynxRDP contributors"
!define HOMEPAGE "https://github.com/guitar24t/lynxrdp"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPNAME}"

Name "${APPNAME} ${VERSION}"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\${APPNAME}"
; Reinstalling over an existing copy should land in the same place.
InstallDirRegKey HKLM "Software\${APPNAME}" "InstallDir"
; Program Files and the machine-wide uninstall key both need administrator.
RequestExecutionLevel admin
SetCompressor /SOLID lzma

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APPNAME}"
VIAddVersionKey "FileDescription" "${APPNAME} installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "CompanyName" "${PUBLISHER}"
VIAddVersionKey "LegalCopyright" "MIT licensed"

!define MUI_ABORTWARNING
!define MUI_ICON "..\..\assets\lynxrdp.ico"
!define MUI_UNICON "..\..\assets\lynxrdp.ico"
!define MUI_FINISHPAGE_RUN "$INSTDIR\lynxrdp.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Open ${APPNAME}"

!insertmacro MUI_PAGE_LICENSE "..\..\LICENSE"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Function .onInit
    ; The client is built for x86_64 only; installing it on 32-bit Windows
    ; would produce a shortcut that fails on launch.
    ${IfNot} ${RunningX64}
        MessageBox MB_ICONSTOP "${APPNAME} requires 64-bit Windows."
        Abort
    ${EndIf}
    SetRegView 64
FunctionEnd

Section "${APPNAME}" SecMain
    SectionIn RO
    SetOutPath "$INSTDIR"
    File "/oname=lynxrdp.exe" "${EXEPATH}"
    File "/oname=LICENSE.txt" "..\..\LICENSE"
    File "/oname=README.md" "..\..\README.md"

    WriteRegStr HKLM "Software\${APPNAME}" "InstallDir" "$INSTDIR"
    WriteUninstaller "$INSTDIR\uninstall.exe"

    ; Add/Remove Programs.
    WriteRegStr   HKLM "${UNINSTKEY}" "DisplayName"     "${APPNAME}"
    WriteRegStr   HKLM "${UNINSTKEY}" "DisplayVersion"  "${VERSION}"
    WriteRegStr   HKLM "${UNINSTKEY}" "DisplayIcon"     "$INSTDIR\lynxrdp.exe"
    WriteRegStr   HKLM "${UNINSTKEY}" "Publisher"       "${PUBLISHER}"
    WriteRegStr   HKLM "${UNINSTKEY}" "URLInfoAbout"    "${HOMEPAGE}"
    WriteRegStr   HKLM "${UNINSTKEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
    WriteRegStr   HKLM "${UNINSTKEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
    WriteRegStr   HKLM "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
    WriteRegDWORD HKLM "${UNINSTKEY}" "NoModify" 1
    WriteRegDWORD HKLM "${UNINSTKEY}" "NoRepair" 1

    ; So Add/Remove Programs shows a size rather than a blank.
    ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
    IntFmt $0 "0x%08X" $0
    WriteRegDWORD HKLM "${UNINSTKEY}" "EstimatedSize" $0
SectionEnd

Section "Start Menu shortcut" SecStartMenu
    CreateDirectory "$SMPROGRAMS\${APPNAME}"
    CreateShortcut "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk" "$INSTDIR\lynxrdp.exe"
    CreateShortcut "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk" "$INSTDIR\uninstall.exe"
SectionEnd

Section /o "Desktop shortcut" SecDesktop
    CreateShortcut "$DESKTOP\${APPNAME}.lnk" "$INSTDIR\lynxrdp.exe"
SectionEnd

LangString DESC_SecMain      ${LANG_ENGLISH} "The ${APPNAME} client. Required."
LangString DESC_SecStartMenu ${LANG_ENGLISH} "Add ${APPNAME} to the Start Menu."
LangString DESC_SecDesktop   ${LANG_ENGLISH} "Put a ${APPNAME} shortcut on the desktop."

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
    !insertmacro MUI_DESCRIPTION_TEXT ${SecMain}      $(DESC_SecMain)
    !insertmacro MUI_DESCRIPTION_TEXT ${SecStartMenu} $(DESC_SecStartMenu)
    !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop}   $(DESC_SecDesktop)
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Function un.onInit
    SetRegView 64
FunctionEnd

Section "Uninstall"
    ; Named deletes rather than RMDir /r: this runs as administrator against
    ; a directory the user chose, and a recursive delete of the wrong path
    ; would be unrecoverable. Saved connections live in %APPDATA% and are
    ; deliberately left alone.
    Delete "$INSTDIR\lynxrdp.exe"
    Delete "$INSTDIR\LICENSE.txt"
    Delete "$INSTDIR\README.md"
    Delete "$INSTDIR\uninstall.exe"
    RMDir "$INSTDIR"

    Delete "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk"
    Delete "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk"
    RMDir "$SMPROGRAMS\${APPNAME}"
    Delete "$DESKTOP\${APPNAME}.lnk"

    DeleteRegKey HKLM "${UNINSTKEY}"
    DeleteRegKey HKLM "Software\${APPNAME}"
SectionEnd
