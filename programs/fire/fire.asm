        org 0x100       ; For .com file.
        ;org 0x7c00      ; For MBR.

section .text
start:
        ; Enter mode 13h: 320x200, 1 byte (256 colors) per pixel.
        mov ax, 0x13
        int 0x10

        ; Make sure es and ds point to our segment (cs).
        push cs
        push cs
        pop ds
        pop es

        ; Write string.
        mov ax, 0x1300          ; ah=13h, al=write mode
        mov bx, 0xf             ; bh=page number (0), bl=attribute (white)
        mov cx, (msg_end - msg) ; cx=length
        mov dx, ((10 << 8) + (40 / 2 - (msg_end - msg) / 2)) ; dh=row, cl=column
        mov bp, msg             ; es:bp=string address
        int 0x10

        ; Set up the palette.
        ; Jare's original FirePal:
        cli             ; No interrupts while we do this, please.
        mov dx, 0x3c8   ; DAC Address Write Mode Register
        xor al, al
        out dx, al      ; Start setting DAC register 0
        inc dx          ; DAC Data Register
        mov cx, (firepal_end - firepal)
        mov si, firepal
setpal1:
        lodsb
        out dx, al      ; Set DAC register (3 byte writes per register)
        loop setpal1
        mov al, 63
        mov cx, (256 * 3 - (firepal_end - firepal))
setpal2:
        out dx, al      ; Set remaining registers to "white heat".
        loop setpal2
        sti             ; Re-enable interrupts.

        ; A buffer at offset 0x1000 from our segment will be used for preparing
        ; the frames. Copy the current framebuffer (the text) there.
        push 0xa000
        pop ds
        push cs
        pop ax
        add ax, 0x1000
        mov es, ax
        xor si, si
        xor di, di
        mov cx, (320 * 200 / 2)
        cld
        rep movsw       ; Copy two bytes at a time.

        push es
        pop ds
mainloop:
        ; On entry to the loop, es and ds should point to the scratch buffer.

        ; Since we'll be working "backwards" through the framebuffer, set the
        ; direction flag, meaning stosb etc. will decrement the index registers.
        std

        ; Let di point to the pixel to be written.
        mov di, (320 * 200 - 1)

        ; Write random values to the bottom row.
        ; For random numbers, use "x = 181 * x + 359" from
        ; Tom Dickens "Random Number Generator for Microcontrollers"
        ; http://home.earthlink.net/~tdickens/68hc11/random/68hc11random.html
        mov cx, 320
        xchg bp, ax     ; Fetch the seed from bp.
bottomrow:
        imul ax, 181
        add ax, 359
        xchg al, ah     ; It's the high 8 bits that are random.
        stosb
        xchg ah, al
        loop bottomrow
        xchg ax, bp     ; Store the seed in bp for next time.

        ; For the next 50 rows, propagate the fire upwards.
        mov cx, (320 * 50)
        mov si, di
        add si, 320     ; si points at the pixel below di.
propagate:
        ; Add the pixel below, below-left, below-right and two steps below.
        xor ax, ax
        mov al, [si]
        add al, [si - 1]
        adc ah, 0
        add al, [si + 1]
        adc ah, 0
        add al, [si + 320]
        adc ah, 0
        imul ax, 15
        ; CSS-DOS patch: `shr ax, 6` is a 186 instruction (C1 /5). Our CPU
        ; core is 8086-only, so use the CL-shift variant (D3 /5) instead.
        ; 3 bytes larger, identical semantics.
        mov cl, 6
        shr ax, cl      ; Compute floor(sum * 15 / 64), averaging and cooling.
        stosb
        dec si
        loop propagate

        ; Mirror some of the fire onto the text.
        mov dx, 15              ; Loop count, decrementing.
        mov di, (90 * 320)      ; Destination pixel.
        mov si, (178 * 320)     ; Source pixel.
mirrorouter:
        mov cx, 320     ; Loop over each pixel in the row.
mirrorinner:
        mov al, [di]    ; Load destination pixel.
        test al, al     ; Check if its zero.
        lodsb           ; Load the source pixel into al.
        jnz mirrorwrite ; For non-zero destination pixel, don't zero al.
        xor al, al
mirrorwrite:
        stosb           ; Write al to the destination pixel.
        loop mirrorinner
        add si, 640     ; Bump si to the row below the one just processed.
        dec dx
        jnz mirrorouter

        ; Sleep for one system clock tick (about 1/18.2 s).
        xor ax, ax
        int 0x1a        ; Returns nbr of clock ticks in cx:dx.
        mov bx, dx
sleeploop:
        xor ax, ax
        int 0x1a
        cmp dx, bx
        je sleeploop

        ; Copy from the scratch buffer to the framebuffer.
        cld
        push 0xa000
        pop es
        mov cx, (320 * (200 - 3) / 2)
        xor si, si
        mov di, (320 * 3)       ; Scroll down three rows to avoid noisy pixels.
        rep movsw

        ; Restore es to point to the scratch buffer.
        push ds
        pop es

        ; Check for key press.
        mov ah, 1
        int 0x16
        jz mainloop

done:
        ; Fetch key from buffer.
        xor ah, ah
        int 0x16

        ; Return to mode 3.
        mov ax, 0x3
        int 0x10

        ; Exit with code 0.
        mov ax, 0x4c00
        int 0x21

; Data.
msg: db 'www.hanshq.net/fire.html'
msg_end:

firepal:
        db     0,   0,   0,   0,   1,   1,   0,   4,   5,   0,   7,   9
        db     0,   8,  11,   0,   9,  12,  15,   6,   8,  25,   4,   4
        db    33,   3,   3,  40,   2,   2,  48,   2,   2,  55,   1,   1
        db    63,   0,   0,  63,   0,   0,  63,   3,   0,  63,   7,   0
        db    63,  10,   0,  63,  13,   0,  63,  16,   0,  63,  20,   0
        db    63,  23,   0,  63,  26,   0,  63,  29,   0,  63,  33,   0
        db    63,  36,   0,  63,  39,   0,  63,  39,   0,  63,  40,   0
        db    63,  40,   0,  63,  41,   0,  63,  42,   0,  63,  42,   0
        db    63,  43,   0,  63,  44,   0,  63,  44,   0,  63,  45,   0
        db    63,  45,   0,  63,  46,   0,  63,  47,   0,  63,  47,   0
        db    63,  48,   0,  63,  49,   0,  63,  49,   0,  63,  50,   0
        db    63,  51,   0,  63,  51,   0,  63,  52,   0,  63,  53,   0
        db    63,  53,   0,  63,  54,   0,  63,  55,   0,  63,  55,   0
        db    63,  56,   0,  63,  57,   0,  63,  57,   0,  63,  58,   0
        db    63,  58,   0,  63,  59,   0,  63,  60,   0,  63,  60,   0
        db    63,  61,   0,  63,  62,   0,  63,  62,   0,  63,  63,   0
firepal_end:

        ; For MBR:
        ;times (510 - ($ - $$)) db 0      ; Pad to 510 bytes
        ;db 0x55                          ; MBR boot signature.
        ;db 0xaa
