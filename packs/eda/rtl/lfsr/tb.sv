module tb;
  logic clk = 0, load, en; logic [15:0] seed; wire [15:0] q;
  lfsr dut(.clk(clk), .load(load), .seed(seed), .en(en), .q(q));
  logic [15:0] r = 16'h3C3C, m;
  int pass = 0, total = 0;
  function automatic logic [15:0] step(input logic [15:0] s); return {s[14:0], s[15] ^ s[13] ^ s[12] ^ s[10]}; endfunction
  initial begin
    for (int i = 0; i < 400; i++) begin
      r = step(step(step(r)));
      load = (i == 0) || (r[5:0] == 0); en = r[6] | r[7]; seed = (i == 50) ? 16'd0 : {r[7:0], r[15:8]};
      #1 clk = 1; #1 clk = 0;
      if (load) m = (seed == 0) ? 16'h0001 : seed; else if (en) m = step(m);
      total++; if (q === m) pass++;
    end
    $display("RESULT %0d %0d", pass, total);
    $finish;
  end
endmodule
