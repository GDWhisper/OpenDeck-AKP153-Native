; Disable USB enhanced power management for Ajazz AKP153 devices (VID 5548, PID 6674).
; The device firmware deadlocks when Windows selectively suspends it (error 0x8007001F),
; requiring a physical replug. Elgato's software does the same for Stream Deck hardware.
; Only effective for per-machine (elevated) installs; the app also self-heals at runtime
; (src/usb_power.rs), which covers per-user installs and new USB port instances.
!macro NSIS_HOOK_POSTINSTALL
  StrCpy $R0 0
  akp153_epm_loop:
    EnumRegKey $R1 HKLM "SYSTEM\CurrentControlSet\Enum\USB\VID_5548&PID_6674" $R0
    StrCmp $R1 "" akp153_epm_done
    WriteRegDWORD HKLM "SYSTEM\CurrentControlSet\Enum\USB\VID_5548&PID_6674\$R1\Device Parameters" "EnhancedPowerManagementEnabled" 0
    IntOp $R0 $R0 + 1
    Goto akp153_epm_loop
  akp153_epm_done:
!macroend
