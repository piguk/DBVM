// Edge detector on a synchronous input: rise (fall) is 1 for one cycle after the sampled input goes from 0 to 1
// (1 to 0). Reset clears the remembered input to 0.
module edge_detect(input clk, input rst, input din, output reg rise, output reg fall);
  reg prev;
  always @(posedge clk) begin
    if (rst) begin
      prev <= 1'b0; rise <= 1'b0; fall <= 1'b0;
    end else begin
      prev <= din;
      rise <= din & ~prev;
      fall <= ~din & prev;
    end
  end
endmodule
