using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Security.Principal;
using System.Text;

public static class Step10LocalUserBroker {
    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)]
    struct StartupInfo {
        public uint cb; public string reserved,desktop,title;
        public uint x,y,xSize,ySize,xCount,yCount,fill,flags;
        public ushort show,reservedSize;
        public IntPtr reservedBytes,input,output,error;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct ProcessInfo { public IntPtr process,thread; public uint pid,tid; }
    [StructLayout(LayoutKind.Sequential,CharSet=CharSet.Unicode)]
    struct ProfileInfo {
        public uint size,flags;
        public string user,profile,defaultPath,server,policy;
        public IntPtr key;
    }
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool GetProcessMitigationPolicy(IntPtr process,int policy,out uint flags,UIntPtr size);
    [DllImport("advapi32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool LogonUserW(string user,string domain,string password,uint type,uint provider,out IntPtr token);
    [DllImport("userenv.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool LoadUserProfileW(IntPtr token,ref ProfileInfo profile);
    [DllImport("userenv.dll",SetLastError=true)] static extern bool UnloadUserProfile(IntPtr token,IntPtr key);
    [DllImport("userenv.dll",SetLastError=true)] static extern bool CreateEnvironmentBlock(out IntPtr environment,IntPtr token,bool inherit);
    [DllImport("userenv.dll",SetLastError=true)] static extern bool DestroyEnvironmentBlock(IntPtr environment);
    [DllImport("advapi32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateProcessAsUserW(IntPtr token,string application,StringBuilder command,IntPtr processAttributes,IntPtr threadAttributes,bool inherit,uint flags,IntPtr environment,string cwd,ref StartupInfo startup,out ProcessInfo process);
    [DllImport("kernel32.dll",SetLastError=true)] static extern uint WaitForSingleObject(IntPtr handle,uint milliseconds);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool GetExitCodeProcess(IntPtr process,out uint code);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool CloseHandle(IntPtr handle);
    static void Require(bool ok,string operation){if(!ok)throw new Win32Exception(Marshal.GetLastWin32Error(),operation);}
    static void Log(string line){File.AppendAllText("C:\\shards-step10-admin\\local-user-api01.txt",DateTime.UtcNow.ToString("o")+" "+line+Environment.NewLine);}
    public static string Policy(){
        uint flags;Require(GetProcessMitigationPolicy(GetCurrentProcess(),16,out flags,new UIntPtr(4)),"GetProcessMitigationPolicy");
        return "RedirectionTrust flags=0x"+flags.ToString("X8");
    }
    public static uint Run(string user,string password,string expectedSid,string encodedChild,uint waitMilliseconds){
        if(WindowsIdentity.GetCurrent().User.Value!="S-1-5-18")throw new InvalidOperationException("Fixture broker must be SYSTEM");
        Log("BROKER="+WindowsIdentity.GetCurrent().Name+" "+Policy());
        IntPtr token=IntPtr.Zero,environment=IntPtr.Zero;
        ProfileInfo profile=new ProfileInfo();ProcessInfo child=new ProcessInfo();bool finished=false;
        try{
            Require(LogonUserW(user,".",password,4,0,out token),"LogonUserW(batch)");
            using(WindowsIdentity identity=new WindowsIdentity(token)){
                if(identity.User.Value!=expectedSid)throw new InvalidOperationException("Unexpected token user");
                if(new WindowsPrincipal(identity).IsInRole(WindowsBuiltInRole.Administrator))throw new InvalidOperationException("Refusing an administrator test token");
                Log("TEST TOKEN="+identity.Name+" SID="+identity.User.Value+" Administrator=False");
            }
            profile.size=(uint)Marshal.SizeOf(typeof(ProfileInfo));profile.flags=1;profile.user=user;
            Require(LoadUserProfileW(token,ref profile),"LoadUserProfileW");
            Require(CreateEnvironmentBlock(out environment,token,false),"CreateEnvironmentBlock");
            string application=Environment.GetFolderPath(Environment.SpecialFolder.System)+"\\WindowsPowerShell\\v1.0\\powershell.exe";
            StringBuilder command=new StringBuilder("\""+application+"\" -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand "+encodedChild);
            StartupInfo startup=new StartupInfo();startup.cb=(uint)Marshal.SizeOf(typeof(StartupInfo));
            Require(CreateProcessAsUserW(token,application,command,IntPtr.Zero,IntPtr.Zero,false,0x08000000|0x400,environment,"C:\\shards-step10",ref startup,out child),"CreateProcessAsUserW");
            Log("CHILD PID="+child.pid);
            uint wait=WaitForSingleObject(child.process,waitMilliseconds);
            if(wait!=0)throw new InvalidOperationException("Child observation wait=0x"+wait.ToString("X8")+"; no cancellation requested");
            finished=true;
            uint code;Require(GetExitCodeProcess(child.process,out code),"GetExitCodeProcess");Log("CHILD EXIT="+code);return code;
        }finally{
            if(child.thread!=IntPtr.Zero)CloseHandle(child.thread);
            if(child.process!=IntPtr.Zero)CloseHandle(child.process);
            if(environment!=IntPtr.Zero)DestroyEnvironmentBlock(environment);
            if(profile.key!=IntPtr.Zero && finished && !UnloadUserProfile(token,profile.key))Log("UnloadUserProfile error="+Marshal.GetLastWin32Error());
            if(token!=IntPtr.Zero)CloseHandle(token);
        }
    }
}
