// Decompile a list of functions (after NamePclntab.java) and dump incoming refs.
//
// Usage:
//   analyzeHeadless <projdir> <projname> -process app.bin -noanalysis \
//     -scriptPath <this dir> -postScript DecompileFunctions.java <targets.tsv> <outdir>
//
// targets.tsv: "hexVA<TAB>label" (label optional). For each target the body is
// disassembled first (with -noanalysis Ghidra has no instructions there, and
// the decompiler then returns an empty result), then decompiled.
//
// Gotcha: use the default simplification style ("decompile"). "normalize" and
// "paramid" complete with no C markup on this Go/ARM build.
import ghidra.app.script.GhidraScript;
import ghidra.app.decompiler.*;
import ghidra.app.cmd.disassemble.DisassembleCommand;
import ghidra.program.model.address.*;
import ghidra.program.model.listing.*;
import ghidra.program.model.symbol.*;
import java.io.*;

public class DecompileFunctions extends GhidraScript {

    public void run() throws Exception {
        String[] args = getScriptArgs();
        if (args.length < 2) {
            println("usage: DecompileFunctions.java <targets.tsv> <outdir>");
            return;
        }
        File outDir = new File(args[1]);
        outDir.mkdirs();
        FunctionManager fm = currentProgram.getFunctionManager();

        int tx = currentProgram.startTransaction("disassemble targets");
        try {
            BufferedReader br = new BufferedReader(new FileReader(args[0]));
            String line;
            while ((line = br.readLine()) != null) {
                int t = line.indexOf('\t');
                long a = Long.parseLong((t < 0 ? line : line.substring(0, t)).trim(), 16);
                Function f = fm.getFunctionAt(toAddr(a));
                if (f == null) continue;
                AddressSetView body = f.getBody();
                if (body.getNumAddresses() > 0) {
                    new DisassembleCommand(body, body, true).applyTo(currentProgram, monitor);
                }
            }
            br.close();
        } finally {
            currentProgram.endTransaction(tx, true);
        }

        DecompInterface ifc = new DecompInterface();
        ifc.toggleCCode(true);
        ifc.toggleSyntaxTree(true);
        ifc.setSimplificationStyle("decompile");
        ifc.openProgram(currentProgram);
        ReferenceManager rm = currentProgram.getReferenceManager();

        PrintWriter refs = new PrintWriter(new FileWriter(new File(outDir, "refs.txt")));
        BufferedReader br = new BufferedReader(new FileReader(args[0]));
        String line;
        while ((line = br.readLine()) != null) {
            int t = line.indexOf('\t');
            long a = Long.parseLong((t < 0 ? line : line.substring(0, t)).trim(), 16);
            String label = (t < 0) ? "" : line.substring(t + 1).trim();
            Address addr = toAddr(a);
            Function f = fm.getFunctionAt(addr);
            if (f == null) { println("MISSING " + Long.toHexString(a)); continue; }
            String fn = String.format("%08x_%s.c", a, f.getName().replaceAll("[^A-Za-z0-9_.]", "_"));
            if (fn.length() > 180) fn = fn.substring(0, 180) + ".c";
            DecompileResults res = ifc.decompileFunction(f, 120, monitor);
            DecompiledFunction df = (res == null) ? null : res.getDecompiledFunction();
            PrintWriter pw = new PrintWriter(new FileWriter(new File(outDir, fn)));
            pw.println("// " + f.getName() + " @ " + Long.toHexString(a) + (label.isEmpty() ? "" : "  [" + label + "]"));
            if (df != null) {
                pw.println(df.getC());
                println("OK " + Long.toHexString(a) + " " + fn);
            } else {
                pw.println("// DECOMP FAILED: " + (res == null ? "null" : res.getErrorMessage()));
                println("FAIL " + Long.toHexString(a));
            }
            pw.close();
            refs.println("=== refs to " + f.getName() + " @" + Long.toHexString(a));
            for (Reference r : rm.getReferencesTo(addr)) {
                Function cf = fm.getFunctionContaining(r.getFromAddress());
                refs.println("   from " + r.getFromAddress() + " in " + (cf == null ? "?" : cf.getName()) + " type=" + r.getReferenceType());
            }
        }
        refs.close();
        println("DONE");
    }
}