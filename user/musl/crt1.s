/* crt1.o - minimal startup for mpos static executables.
 * Provides _start which sets up argc/argv and calls main.
 */
.text
.globl _start
_start:
    /* Set up the stack: [rsp] = argc, [rsp+8] = argv[0], ... */
    xorl %ebp, %ebp
    movq (%rsp), %rdi        /* argc */
    leaq 8(%rsp), %rsi       /* argv */
    leaq 16(%rsp,%rdi,8), %rdx  /* envp = argv + argc + 1 */

    /* Clear the direction flag as per ABI */
    cld

    /* Call main(argc, argv, envp) */
    call main

    /* exit with main's return value */
    movq %rax, %rdi
    movq $60, %rax           /* SYS_exit */
    syscall
