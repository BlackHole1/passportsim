    .section .text.start, "ax"
    .globl _start
_start:
    /* a0 = iteration count, preloaded by the host emulator */
    la sp, _stack_top
    la t0, __bss_start
    la t1, __bss_end
1:  bgeu t0, t1, 2f
    sw zero, 0(t0)
    addi t0, t0, 4
    j 1b
2:  call bench_main
    /* a0 = checksum; exit via ecall (a7 = 93) */
    li a7, 93
    ecall
3:  j 3b
