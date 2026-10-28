// 16-bit Fibonacci LFSR, taps 16, 14, 13, 11 (shifting left, the new bit enters at bit 0). load sets the state
// to seed; an all-zero seed is replaced by 16'h0001, as zero never changes.
module lfsr(input clk, input load, input [15:0] seed, input en, output reg [15:0] q);
  wire fb = q[15] ^ q[13] ^ q[12] ^ q[10];
  always @(posedge clk) begin
    if (load) q <= (seed == 16'd0) ? 16'h0001 : seed;
    else if (en) q <= {q[14:0], fb};
  end
endmodule
