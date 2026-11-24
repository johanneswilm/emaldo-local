// Create every Go function from a pclntab dump, with exact body boundaries.
//
// Usage (Ghidra 12.2 headless; the app.bin project is imported but its
// auto-analysis never creates functions at most Go entry points):
//   analyzeHeadless <projdir> <projname> -process app.bin -noanalysis \
//     -scriptPath <this dir> -postScript NamePclntab.java <funcs.tsv>
//
// funcs.tsv: "hexVA<TAB>full.symbol.Name" sorted by VA, produced by
//   scripts/pclntab_dump.py --tsv.
// Body = [entry, next_entry - 1]: Go lays text out in functab order, so this
// is exact (verified on the 41,908-entry table). Existing auto-analysis
// functions that overlap are removed first; createFunction would otherwise
// refuse. Names are sanitized for Ghidra (Go symbols contain / ( ) * etc.).
import ghidra.app.script.GhidraScript;
import ghidra.program.model.address.*;
import ghidra.program.model.listing.*;
import ghidra.program.model.mem.MemoryBlock;
import ghidra.program.model.symbol.SourceType;
import java.io.*;
import java.util.*;

public class NamePclntab extends GhidraScript {

    private String sanitize(String n) {
        String s = n.replaceAll("[^A-Za-z0-9_.$]", "_");
        if (s.length() > 400) s = s.substring(0, 400);
        if (s.isEmpty() || Character.isDigit(s.charAt(0))) s = "_" + s;
        return s;
    }

    private long textEnd() {
        long end = 0;
        for (MemoryBlock b : currentProgram.getMemory().getBlocks()) {
            if (b.isExecute()) end = Math.max(end, b.getEnd().getOffset());
        }
        return end;
    }

    public void run() throws Exception {
        String[] args = getScriptArgs();
        if (args.length < 1) {
            println("usage: NamePclntab.java <funcs.tsv>");
            return;
        }
        List<long[]> addrs = new ArrayList<>();
        List<String> names = new ArrayList<>();
        BufferedReader br = new BufferedReader(new FileReader(args[0]));
        String line;
        while ((line = br.readLine()) != null) {
            int t = line.indexOf('\t');
            if (t <= 0) continue;
            addrs.add(new long[]{Long.parseLong(line.substring(0, t).trim(), 16)});
            names.add(line.substring(t + 1).trim());
        }
        br.close();

        FunctionManager fm = currentProgram.getFunctionManager();
        long end = textEnd();
        int created = 0, removed = 0, failed = 0, skipped = 0;
        int tx = currentProgram.startTransaction("pclntab functions");
        boolean commit = false;
        try {
            for (int i = 0; i < addrs.size(); i++) {
                long a = addrs.get(i)[0];
                long next = (i + 1 < addrs.size()) ? addrs.get(i + 1)[0] : end + 1;
                if (next <= a) { skipped++; continue; }
                Address entry = toAddr(a);
                AddressSet body = new AddressSet(entry, toAddr(next - 1));
                if (entry.getOffset() < currentProgram.getMinAddress().getOffset()) { skipped++; continue; }
                for (Function f : fm.getFunctions(body, true)) {
                    try { fm.removeFunction(f.getEntryPoint()); removed++; } catch (Exception e) { }
                }
                try {
                    Function nf = fm.createFunction(sanitize(names.get(i)), entry, body, SourceType.USER_DEFINED);
                    if (nf == null) failed++; else created++;
                } catch (Exception e) {
                    failed++;
                }
                if ((i % 5000) == 0) println("progress " + i + " created=" + created + " removed=" + removed + " failed=" + failed);
            }
            commit = true;
        } finally {
            currentProgram.endTransaction(tx, commit);
        }
        println("RESULT created=" + created + " removed=" + removed + " failed=" + failed + " skipped=" + skipped);
        println("TOTALFUNCS=" + fm.getFunctionCount());
    }
}