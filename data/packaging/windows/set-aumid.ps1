# Set the System.AppUserModel.ID property on a .lnk (todo 21).
#
# NSIS's CreateShortcut cannot write shell properties, and the System plugin's
# raw COM plumbing silently no-ops on the property store, so the installer runs
# this helper: it loads the shortcut through IPersistFile, sets
# PKEY_AppUserModel_ID via IPropertyStore, commits and saves. The property is
# REQUIRED: the toast transport (todo 16) attributes the daemon's
# notifications to the app id io.github.zbndev.Tidemark through it.
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File set-aumid.ps1 -Lnk <path> -Aumid <id>

param(
  [Parameter(Mandatory = $true)][string] $Lnk,
  [Parameter(Mandatory = $true)][string] $Aumid
)

$ErrorActionPreference = 'Stop'

$source = @'
using System;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;

public class TidemarkAumid {
  [StructLayout(LayoutKind.Sequential)]
  public struct PROPERTYKEY { public Guid fmtid; public uint pid; }

  [StructLayout(LayoutKind.Sequential)]
  public struct PropVariant {
    public ushort vt;
    public ushort r1;
    public ushort r2;
    public ushort r3;
    public IntPtr p;
    public IntPtr p2;
  }

  [ComImport, Guid("00021401-0000-0000-C000-000000000046")]
  private class ShellLink {}

  [ComImport, Guid("886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  private interface IPropertyStore {
    void GetCount(out uint count);
    void GetAt(uint index, out PROPERTYKEY key);
    void GetValue(ref PROPERTYKEY key, out PropVariant pv);
    void SetValue(ref PROPERTYKEY key, ref PropVariant pv);
    void Commit();
  }

  private const ushort VT_LPWSTR = 31;
  private static readonly PROPERTYKEY AppUserModelIdKey =
    new PROPERTYKEY { fmtid = new Guid("9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3"), pid = 5 };

  public static void Set(string lnk, string aumid) {
    IPersistFile file = (IPersistFile)(object)new ShellLink();
    file.Load(lnk, 2 /* STGM_READWRITE */);
    IPropertyStore store = (IPropertyStore)file;
    // a local copy: C# forbids passing a static readonly field by ref
    PROPERTYKEY key = AppUserModelIdKey;
    PropVariant pv = new PropVariant();
    pv.vt = VT_LPWSTR;
    pv.p = Marshal.StringToCoTaskMemUni(aumid);
    try {
      store.SetValue(ref key, ref pv);
      store.Commit();
    } finally {
      Marshal.FreeCoTaskMem(pv.p);
    }
    ((IPersistFile)store).Save(lnk, true);
  }
}
'@

Add-Type -TypeDefinition $source -Language CSharp
[TidemarkAumid]::Set($Lnk, $Aumid)
Write-Output "AUMID '$Aumid' set on $Lnk"
