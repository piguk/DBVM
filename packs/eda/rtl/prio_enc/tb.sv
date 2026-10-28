module tb;
  logic [15:0] x; wire [3:0] y; wire valid;
  prio_enc dut(.x(x), .y(y), .valid(valid));
  logic [15:0] r = 16'h5A5A;
  int pass = 0, total = 0;
  function automatic logic [15:0] step(input logic [15:0] s); return {s[14:0], s[15] ^ s[13] ^ s[12] ^ s[10]}; endfunction
  function automatic logic [3:0] model(input logic [15:0] x);
    for (int i = 15; i >= 0; i--) if (x[i]) return 4'(i);
    return 0;
  endfunction
  initial begin
    for (int i = 0; i < 400; i++) begin
      r = step(step(step(r)));
      x = (i % 4 == 0) ? 16'(1) << i[3:0] : (i % 4 == 1) ? r >> r[3:0] : (i == 2) ? 16'd0 : r;
      #1;
      total++; if (y === model(x) && valid === (x != 0)) pass++;
    end
    $display("RESULT %0d %0d", pass, total);
    $finish;
  end
endmodule
