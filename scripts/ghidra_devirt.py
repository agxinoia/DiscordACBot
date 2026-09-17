# Ghidra Headless Devirtualization & Disassembly Analysis Script
# @category Decompilation
# @author EAC Tracker Pipeline

import json
import os
import sys

from ghidra.app.decompiler import DecompileOptions, DecompInterface
from ghidra.util.task import ConsoleTaskMonitor

def analyze_program():
    monitor = ConsoleTaskMonitor()
    program = currentProgram
    listing = program.getListing()
    decomp_interface = DecompInterface()
    decomp_interface.openProgram(program)

    report = {
        "program_name": program.getName(),
        "image_base": str(program.getImageBase()),
        "language": str(program.getLanguageID()),
        "compiler": str(program.getCompilerSpec().getCompilerSpecID()),
        "total_functions": program.getFunctionManager().getFunctionCount(),
        "analyzed_functions": [],
        "vm_candidates": [],
        "imports": [],
        "exports": [],
    }

    # Extract symbols
    symbol_table = program.getSymbolTable()
    for sym in symbol_table.getAllSymbols(True):
        if sym.isExternal():
            report["imports"].append(sym.getName())
        elif sym.isGlobal() and not sym.isExternal():
            report["exports"].append({"name": sym.getName(), "address": str(sym.getAddress())})

    report["imports"] = report["imports"][:30]
    report["exports"] = report["exports"][:30]

    # Inspect functions for potential VM dispatchers / obfuscated logic
    func_mgr = program.getFunctionManager()
    funcs = list(func_mgr.getFunctions(True))

    for func in funcs:
        body = func.getBody()
        instruction_count = 0
        indirect_jumps = 0
        has_switch = False
        inst_iter = listing.getInstructions(body, True)

        while inst_iter.hasNext():
            inst = inst_iter.next()
            instruction_count += 1
            mnemonic = inst.getMnemonicString().lower()
            if mnemonic in ["jmp", "call"]:
                # Check for indirect call/jump (e.g. jmp [rax], jmp rax)
                if inst.getNumOperands() > 0:
                    op_type = inst.getOperandType(0)
                    if op_type & 0x04 or op_type & 0x40:  # Register or indirect
                        indirect_jumps += 1

        # Heuristic scoring for VM dispatcher or obfuscated handler
        is_vm_candidate = (indirect_jumps >= 1 and instruction_count > 15) or instruction_count > 80

        if is_vm_candidate and len(report["vm_candidates"]) < 5:
            # Decompile candidate
            res = decomp_interface.decompileFunction(func, 30, monitor)
            decomp_c = ""
            if res.decompileCompleted():
                decomp_c = res.getDecompiledFunction().getC()

            report["vm_candidates"].append({
                "name": func.getName(),
                "entry": str(func.getEntryPoint()),
                "instruction_count": instruction_count,
                "indirect_jumps": indirect_jumps,
                "decompiled_c": decomp_c[:3000]
            })

    output_path = os.environ.get("GHIDRA_DEVIRT_OUT", "devirt_report.json")
    with open(output_path, "w") as f:
        json.dump(report, f, indent=2)
    print("Devirtualization report exported to " + output_path)

if __name__ == "__main__":
    analyze_program()
