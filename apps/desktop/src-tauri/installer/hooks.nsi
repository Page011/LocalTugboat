; Tugboat NSIS hooks (Tauri 2 installerHooks). The installer is per-machine (elevated), so it
; may add a Windows Firewall rule: inbound UDP for the Tugboat program on the Private and
; Public profiles. Program-scoped, no port number, because the UDP port is a setting.
; The app checks at run time through INetFwPolicy2 that this rule exists and takes effect
; (group policy can ignore it) and shows a banner if not (src/firewall.rs).
;
; The delete before the add makes a reinstall or upgrade idempotent: exactly one rule.

!define TUGBOAT_RULE "Local Tugboat"

!macro NSIS_HOOK_POSTINSTALL
  nsExec::ExecToLog 'netsh advfirewall firewall delete rule name="${TUGBOAT_RULE}" program="$INSTDIR\${MAINBINARYNAME}.exe"'
  Pop $0
  nsExec::ExecToLog 'netsh advfirewall firewall add rule name="${TUGBOAT_RULE}" dir=in action=allow program="$INSTDIR\${MAINBINARYNAME}.exe" enable=yes profile=private,public protocol=UDP'
  Pop $0
  ${If} $0 != 0
    ; Not fatal: the app shows the firewall banner with a [Fix] button.
    DetailPrint "Could not add the Windows Firewall rule (code $0). Tugboat will offer to add it."
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::ExecToLog 'netsh advfirewall firewall delete rule name="${TUGBOAT_RULE}" program="$INSTDIR\${MAINBINARYNAME}.exe"'
  Pop $0
!macroend
