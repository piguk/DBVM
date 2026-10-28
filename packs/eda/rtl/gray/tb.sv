module tb;
  logic [7:0] b, h; wire [7:0] g, back;
  gray dut(.b(b), .g(g), .h(h), .back(back));
  logic [15:0] r = 16'h0F0F;
  int pass = 0, total = 0;
  function automatic logic [15:0] step(input logic [15:0] s); return {s[14:0], s[15] ^ s[13] ^ s[12] ^ s[10]}; endfunction
  function automatic logic [7:0] to_bin(input logic [7:0] h);
    logic [7:0] v = h; for (int s = 1; s < 8; s <<= 1) v ^= v >> s; return v;
  endfunction
  initial begin
    for (int i = 0; i < 300; i++) begin
      r = step(step(r)); b = (i < 256) ? 8'(i) : r[7:0]; h = r[15:8];
      #1;
      total++; if (g === (b ^ (b >> 1)) && back === to_bin(h)) pass++;
    end
    $display("RESULT %0d %0d", pass, total);
    $finish;
  end
endmodule
